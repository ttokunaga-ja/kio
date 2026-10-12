mod index;
mod open_view;
mod search;
use crate::commands::AdapterArgs;
use crate::commands::AdapterCommand;
use crate::commands::BatchArgs;
use crate::commands::BatchCommand;
use crate::commands::CommandRequest as Command;
use crate::commands::IndexArgs;
use crate::commands::LedgerArgs;
use crate::commands::LedgerCommand;
use crate::commands::ReindexArgs;
use crate::commands::SnapshotAction;
use crate::commands::SnapshotArgs;
use crate::context::AppContext;
use crate::context::EvidenceRegistry;
use crate::embedding_owners::RetainedEmbeddingChunk;
use crate::historical_reindex::RetainedNormalizedInstance;
use crate::historical_reindex::pinned_manifest_is_complete;
use crate::historical_reindex::retained_history_instances;
use crate::index::chunk_ledger::ensure_purge_epoch_initialized;
use crate::index::chunk_ledger::recover_index_generation;
use crate::index::chunk_ledger::rotate_index_generation_unconditionally;
use crate::index::chunk_ledger::sqlite_path;
use crate::index::rebuild::attach_skipped_units;
use crate::index::rebuild::held_secret_embedding_chunk_ids;
use crate::index::rebuild::is_rebuild_skippable_unit_error;
use crate::index::rebuild::read_head_tree_for_rebuild;
use crate::index::rebuild::rebuild_step3_index;
use crate::index::rebuild::store_corrupt_error;
use crate::invocation::Invocation;
use crate::invocation::InvocationRuntime;
use crate::invocation::execute_lifecycle;
use crate::invocation::exit_override_error_code;
use crate::invocation::merged_index_exit_override;
use crate::invocation::peek_exit_override;
use crate::management::ChildScope;
use crate::management::binding_for_repo;
use crate::management::open_existing_managed_root;
use crate::management::reconcile_planned_child;
use crate::ocr_discovery::prepared_units_from_ocr_discovery;
use crate::ocr_discovery::supports_ocr_from_scratch;
use crate::online_task::targets_standard_online_markdownize;
use crate::open_view::cas_object_path;
use crate::open_view::existing_cas_object_path;
use crate::open_view::run_open;
use crate::open_view::run_view;
use crate::promotion::apply_online_promotion_to_index;
use crate::promotion::finish_pending_online_promotion;
use crate::promotion::promote_completed_online_markdownize;
use crate::promotion::recover_pending_online_promotion;
use crate::repair_command::run_repair;
use crate::scope_access::ensure_raw_ingest_allowed;
use crate::scope_access::maybe_wait_at_test_search_final_consent_barrier;
use crate::scope_access::raw_ingest_is_purge_blocked;
use crate::scope_access::register_scope;
use crate::scope_access::registry_duplicate_guard;
use crate::scope_access::resolve_evidence_scope_target;
use crate::scope_access::scope_id;
use crate::scope_commands::run_init;
use crate::scope_commands::run_status;
use crate::search::command::run_search;
use crate::search::projection::mark_replica_rebuilding_or_log;
use crate::search::projection::mark_replica_unavailable_or_log;
use crate::search::projection::maybe_inject_replica_after_head_fault;
use crate::search::projection::publish_in_place_delta;
use crate::search::projection::replica_now_ms;
use crate::search::projection::replica_scope_stamp;
use crate::search::projection::write_through_projection_or_log;
use crate::search::projection::write_through_projection_or_log_for_repo;
use crate::search::query::ensure_snapshot_tree_entries;
use crate::search::query::snapshot_chunking_config_hash;
use crate::search::scope::ExecScope;
use crate::search::scope::SearchedScopeInfo;
use crate::search::scope::aggregator_path;
use crate::search::scope::current_embedding_policy;
use crate::search::scope::revalidate_current_policy_map;
use crate::settings::adapter_runtime_settings_from_user_config;
use crate::settings::batch_recovery_context;
use crate::settings::bbox_annotation_enabled;
use crate::settings::data_home;
use crate::settings::effective_incremental_config;
use crate::settings::effective_max_input_bytes;
use crate::settings::ensure_device_dirs_resolvable;
use crate::settings::require_repo_tool_lock;
use crate::settings::user_config_toml_path;
use crate::settings::validate_repo_tool_lock;
use crate::settings::validate_user_config;
use crate::settings::validate_user_tools_config;
use crate::snapshot_history::run_diff;
use crate::snapshot_history::run_inspect;
use crate::snapshot_history::run_log;
use crate::snapshot_history::run_snapshot_auto;
use crate::snapshot_history::run_snapshot_create;
use crate::snapshot_history::run_tag;
use kio_adapter::BatchOcrMaterializationError;
use kio_adapter::batch_client::BatchJobStatus;
use kio_adapter::batch_client::BatchOutputLine;
use kio_adapter::batch_client::MistralBatchClient;
use kio_adapter::batch_client::batch_input_line;
use kio_adapter::batch_client::batch_upload_filename;
use kio_adapter::batch_client::configured_mistral_batch_client_with_control;
use kio_adapter::batch_client::ocr_batch_body;
use kio_adapter::catalog::DeclaredEmbeddingProfile;
use kio_adapter::catalog::EmbeddingExecution;
use kio_adapter::catalog::StandardOnlineMarkdownizeRequest;
use kio_adapter::catalog::active_embedding_execution_with_control;
use kio_adapter::catalog::active_local_ocr_execution_with_control;
use kio_adapter::catalog::builtin_offline_markdownize_adapter;
use kio_adapter::catalog::builtin_prepare_profile;
use kio_adapter::catalog::convert_office_to_pdf_with_control;
use kio_adapter::catalog::declared_embedding_profile_for;
use kio_adapter::catalog::effective_prepared_unit_hints;
use kio_adapter::catalog::embedding_adapter_profile_with_control;
use kio_adapter::catalog::embedding_preferred_request_kind_with_control;
use kio_adapter::catalog::local_ocr_handles_media_type;
use kio_adapter::catalog::local_ocr_markdownize_adapter_with_control;
use kio_adapter::catalog::resolve_standard_online_markdownize_profile_with_bbox_with_control;
use kio_adapter::catalog::run_embedding_with_control;
use kio_adapter::catalog::run_standard_online_markdownize_with_bytes_with_control;
use kio_adapter::catalog::standard_online_markdownize_profile;
use kio_adapter::catalog::standard_online_markdownize_profile_with_bbox;
#[cfg(debug_assertions)]
use kio_adapter::identity::tool_profile_hash;
use kio_adapter::local_ocr_markdownize::LocalOcrExecution;
use kio_adapter::materialize_mistral_batch_ocr_body;
use kio_adapter::office_convert::is_office_media;
use kio_adapter::office_convert::resolve_office_converter_with_control;
use kio_adapter::tool_lock::AdapterRuntimeSettings;
use kio_adapter::tool_lock::load_tool_lock;
use kio_adapter::traits::MarkdownizeAdapter;
use kio_adapter::traits::PreferredRequestKind;
#[cfg(debug_assertions)]
use kio_adapter::types::AdapterKind;
use kio_adapter::types::AdapterProfile;
use kio_adapter::types::EmbeddingContent;
use kio_adapter::types::EmbeddingInputType;
use kio_adapter::types::EmbeddingItem;
use kio_adapter::types::ExecutionMode;
use kio_adapter::types::MAX_EMBEDDING_ITEM_BYTES;
use kio_adapter::types::MarkdownUnit;
use kio_adapter::types::MarkdownizeMode as AdapterMarkdownizeMode;
use kio_adapter::types::MarkdownizeRequest;
use kio_adapter::types::PreparedUnitHint;
use kio_adapter::types::PreviousMarkdownizeContext;
use kio_adapter::types::RawInput;
use kio_adapter::types::UnitKind;
use kio_core::ExitCode;
use kio_core::KioError;
use kio_core::Result;
use kio_core::cas::ContentObjectKind;
use kio_core::cas::canonical_json_bytes;
use kio_core::cas::is_hash;
use kio_core::control::ControlHandle;
use kio_core::dag::NormalizeRef;
use kio_core::scope::PendingNormalizeRef;
use kio_core::scope::Repository;
use kio_core::scope::new_ulid;
#[cfg(debug_assertions)]
use kio_core::test_control::DebugTestControl;
#[cfg(debug_assertions)]
use kio_core::test_control::MarkdownizeAdapterMode;
use kio_index::EmbeddingDistance;
use kio_index::EmbeddingModality;
use kio_index::EmbeddingTargetType;
use kio_index::embedding_store;
use kio_index::embedding_store::f32_to_le_bytes;
use kio_index::fts::ExistingSourceIndexOpenMode;
use kio_index::fts::FtsSchemaConfig;
use kio_index::fts::FtsTokenizer;
use kio_index::fts::open_existing_source_index_connection;
use kio_index::registry::RegistryDb;
use kio_pipeline::budget::BudgetCapKind;
use kio_pipeline::budget::BudgetCaps;
use kio_pipeline::budget::BudgetEstimate;
use kio_pipeline::budget::budget_warning;
use kio_pipeline::budget::estimate_local_baseline_cost;
use kio_pipeline::budget::evaluate_budget_with_caps;
use kio_pipeline::budget::read_budget_policy;
use kio_pipeline::budget::utc_month;
use kio_pipeline::ledger::BatchRequestRow;
use kio_pipeline::ledger::BatchState;
use kio_pipeline::ledger::LedgerDb;
use kio_pipeline::ledger::LedgerReadSnapshot;
use kio_pipeline::ledger::LedgerSnapshotError;
use kio_pipeline::ledger::Outcome;
use kio_pipeline::ledger::RequestKind;
use kio_pipeline::ledger::TaskKey as LedgerTaskKey;
use kio_pipeline::ledger::ops::AbandonExecution;
use kio_pipeline::ledger::ops::AbandonResolution;
use kio_pipeline::ledger::ops::AbandonSelector;
use kio_pipeline::ledger::ops::BilledAmount;
use kio_pipeline::ledger::ops::BudgetCapConfig;
use kio_pipeline::ledger::ops::ClaimOutcome;
use kio_pipeline::ledger::ops::TerminalWrite;
use kio_pipeline::ledger::ops::authorize_unknown_sync_resend;
use kio_pipeline::ledger::ops::batch_poll_candidates;
use kio_pipeline::ledger::ops::device_input_hash;
use kio_pipeline::ledger::ops::execute_abandon;
use kio_pipeline::ledger::ops::get_batch_request;
use kio_pipeline::ledger::ops::ledger_month_total;
use kio_pipeline::ledger::ops::phase2a_record_provider_scope;
use kio_pipeline::ledger::ops::phase2a_record_upload_id;
use kio_pipeline::ledger::ops::phase2b_record_job_create_started;
use kio_pipeline::ledger::ops::phase2b_record_job_created;
use kio_pipeline::ledger::ops::recovery_finish_cleanup;
use kio_pipeline::ledger::ops::recovery_settle_unknown;
use kio_pipeline::ledger::ops::reset_contract_violations;
use kio_pipeline::ledger::ops::resolve_abandon_selector;
use kio_pipeline::ledger::ops::resolve_billing_from_reported_usage;
use kio_pipeline::ledger::ops::sync_recovery_candidates;
use kio_pipeline::ledger::ops::terminal_transaction;
use kio_pipeline::ledger::ops::uuid_v7_timestamp_millis;
use kio_pipeline::markdownize::IncrementalHints;
use kio_pipeline::markdownize::IncrementalModeDecision;
use kio_pipeline::markdownize::IncrementalModeInput;
use kio_pipeline::markdownize::MarkdownizeMode;
use kio_pipeline::markdownize::NormalizedInstanceManifest;
use kio_pipeline::markdownize::NormalizedUnitManifestEntry;
use kio_pipeline::markdownize::NormalizedUnitObject;
use kio_pipeline::markdownize::UnitStatus;
use kio_pipeline::markdownize::ValidatedNormalizedInstance;
use kio_pipeline::markdownize::choose_markdownize_mode;
use kio_pipeline::markdownize::load_validated_normalized_instance_with_control;
use kio_pipeline::markdownize::persist_normalized_instance_with_control;
use kio_pipeline::markdownize::validate_markdownize_response;
use kio_pipeline::policy::CurrentPolicyEvaluator;
use kio_pipeline::prepare::PrepareStageBytesRequest;
use kio_pipeline::prepare::PreparedUnit;
use kio_pipeline::prepare::UnitFingerprint;
use kio_pipeline::prepare::UnitType;
use kio_pipeline::prepare::hash_bytes;
use kio_pipeline::prepare::map_units;
use kio_pipeline::prepare::pdf_text_pages_bounded;
use kio_pipeline::prepare::prepare_units_from_bytes_with_control;
use kio_pipeline::prepare::unit_ref;
use kio_pipeline::scan::CHILD_SCOPE_DISCOVERY_PAGE_SIZE;
use kio_pipeline::scan::ScanCandidate;
use kio_pipeline::scan::ScanPreview;
use kio_pipeline::scan::ScanPreviewRequest;
use kio_pipeline::scan::bind_planned_child;
use kio_pipeline::scan::build_managed_scan_preview;
use kio_pipeline::scan::build_planned_child_scan_preview;
use kio_pipeline::scan::build_scan_preview;
use kio_pipeline::scan::classify_secret;
use kio_pipeline::scan::discover_managed_child_scopes_page;
use kio_pipeline::scan::managed_scan_policy_allows_file;
use kio_pipeline::scan::read_bound_verified_scan_input;
use kio_pipeline::task::EmbeddingWorkKey;
use kio_pipeline::task::HoldReason;
use kio_pipeline::task::LEDGER_INITIALIZATION_REQUIRED_REASON;
use kio_pipeline::task::RESULT_UNKNOWN_REASON;
use kio_pipeline::task::RetryErrorKind;
use kio_pipeline::task::TaskDescriptor;
use kio_pipeline::task::TaskOutputRef;
use kio_pipeline::task::TaskStatus;
use kio_pipeline::task::TaskStore;
use kio_pipeline::task::TaskType;
use kio_pipeline::task::hold_reason_for_reason;
use kio_pipeline::task::normalized_task_output_ref;
use kio_pipeline::task::retry_policy;
use kio_pipeline::task::task_can_complete_from_materialized_output;
use kio_pipeline::task::task_can_enter_secret_hold;
use kio_pipeline::task::task_status_from_unit_counts;
use kio_pipeline::task::validate_task_output_ref;
use kio_pipeline::unsupported::UNSUPPORTED_REASON_RESOLVED;
use kio_pipeline::unsupported::UNSUPPORTED_REASON_UNRECOGNIZED_BINARY;
use kio_pipeline::unsupported::UnsupportedInputDisposition;
use kio_pipeline::unsupported::UnsupportedInputStore;
use kio_search::object_uri::extract_related_images;
use rusqlite::Connection;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process;
use std::sync::Arc;

mod invocation;
mod repair_command;
mod scope_access;
mod scope_commands;
mod settings;
mod snapshot_history;

mod approvals;
mod batch_recovery_key;
#[cfg(test)]
mod contextual_embedding_tests;
mod cursor_key;
mod discovery;
mod embedding_owners;
mod export;
#[cfg(test)]
mod test_support;

mod gc;
mod grants;
mod historical_reindex;
mod image_authority;
mod image_ingest;
mod managed_restore;
mod management;
mod multi_scope;
mod ocr_discovery;
mod online_task;
mod platform;
mod private_state;
mod promotion;
mod purge;
mod root_registration;
mod search_history;
mod search_time;
mod trust;
mod verify_objects;
pub mod watch;
mod watch_command;
mod watch_service;

/// 07-adapter-spec.md §5.1: `UnsupportedInputDisposition.reason` recorded for an
/// Office (DOCX/PPTX) file when no converter (`KIO_OFFICE_CONVERTER` / PATH
/// `soffice`) resolves at `kio index` time -- no online task is enqueued for it
/// (no doomed task), and `compute_index_status`'s `office_conversion_unavailable`
/// surfaces it. Shares the `UnsupportedInputStore` (`UNSUPPORTED_INPUTS_FILE`)
/// used for `UNSUPPORTED_REASON_UNRECOGNIZED_BINARY` -- `reason` is a free-form
/// `String`, not a closed enum, so this is a plain sibling value, not a change to
/// `kio-pipeline`.
const OFFICE_CONVERSION_UNAVAILABLE_REASON: &str = "office_conversion_unavailable";

pub mod commands;
pub mod context;

pub fn execute(ctx: &AppContext, command: Command) -> Result<Value> {
    #[cfg(debug_assertions)]
    let control = kio_core::control::ControlHandle::new(DebugTestControl::from_env());
    #[cfg(not(debug_assertions))]
    let control = kio_core::control::ControlHandle::default();
    // The Windows scheduler runner carries an already absolute, owner-private
    // manifest locator. It must bind that locator before reading any inherited
    // device-directory environment, because Task Scheduler does not preserve
    // the interactive shell's XDG variables.
    if matches!(
        &command,
        Command::Watch(commands::WatchArgs {
            command: commands::WatchCommand::Service(commands::WatchServiceCommand::Runner { .. },),
            ..
        })
    ) {
        return execute_lifecycle(ctx, &control, command);
    }
    // R13-6: refuse to run if device-global state cannot resolve to an absolute
    // base dir (no absolute $HOME and no absolute $XDG_*), before any command
    // reads or writes the registry / logs / cost ledger / cursor-key.
    ensure_device_dirs_resolvable(&control)?;
    // Lifecycle control remains available when adapter settings are broken.
    // These operations neither execute content adapters nor grant egress.
    if matches!(
        &command,
        Command::Watch(commands::WatchArgs {
            command: commands::WatchCommand::Status
                | commands::WatchCommand::Stop
                | commands::WatchCommand::Service(_),
            ..
        }) | Command::Adapter(AdapterArgs {
            command: Some(AdapterCommand::Revoke(_) | AdapterCommand::Trust(_)),
        }) | Command::Ledger(LedgerArgs {
            command: Some(
                LedgerCommand::Init { .. }
                    | LedgerCommand::Status
                    | LedgerCommand::Backup(_)
                    | LedgerCommand::Restore(_)
                    | LedgerCommand::Recover,
            ),
        })
    ) {
        return execute_lifecycle(ctx, &control, command);
    }
    validate_user_tools_config(&control)?;
    validate_user_config(&control)?;
    let mut settings = adapter_runtime_settings_from_user_config(&control)?;
    let managed_trust = trust::acquire_runtime_trust(&trust::directory_path(&control), &control)?;
    if let Some(trust) = managed_trust.as_ref() {
        settings.local_peer_ca_path = Some(trust.path().to_path_buf());
        settings.local_peer_trust_digest = Some(trust.digest().to_owned());
        settings.local_peer_trust_binding = Some(trust.binding().to_owned());
    }
    let runtime = InvocationRuntime {
        locks: kio_core::scope::LockSession::default(),
        control,
        settings: Arc::new(settings),
        _managed_trust: managed_trust,
    };
    execute_dispatch(
        &Invocation {
            context: ctx,
            settings: &runtime.settings,
            control: &runtime.control,
            locks: &runtime.locks,
        },
        command,
    )
}

fn execute_dispatch(ctx: &Invocation<'_>, command: Command) -> Result<Value> {
    let adapter_settings = ctx.settings;

    match command {
        Command::RootRegister(args) => root_registration::run(ctx, args),
        Command::Init(args) => run_init(ctx, args),
        Command::Status => run_status(ctx),
        Command::Snapshot(SnapshotArgs {
            action: SnapshotAction::Auto,
        }) => run_snapshot_auto(ctx),
        Command::Snapshot(SnapshotArgs {
            action: SnapshotAction::Create(args),
        }) => run_snapshot_create(ctx, args, adapter_settings),
        Command::Log(args) => run_log(ctx, args),
        Command::Diff(args) => run_diff(ctx, args),
        Command::Inspect(args) => run_inspect(ctx, args),
        Command::Tag(args) => run_tag(ctx, args),
        Command::Index(args) => run_index(ctx, args),
        Command::Watch(args) => watch_command::run(ctx, args, ctx.control),
        Command::Batch(args) => run_batch(ctx, args),
        Command::Adapter(args) => run_adapter(ctx, args),
        Command::Ledger(args) => run_ledger(adapter_settings, args, ctx.control),
        Command::Repair(args) => run_repair(ctx, args),
        Command::Gc(args) => gc::run(ctx, args.dry_run, args.yes, args.prune_unreachable, false),
        Command::Search(args) => run_search(ctx, args),
        Command::Open(args) => run_open(ctx, args),
        Command::View(args) => run_view(ctx, args),
        Command::Reindex(args) => run_reindex(ctx, args),
        Command::Evidence(args) => verify_objects::run_evidence(ctx, args),
        Command::Export(args) => export::run(ctx, args),
        Command::Restore(args) => managed_restore::run(ctx, args),
        Command::Purge(args) => purge::run(ctx, args),
    }
}

fn run_index(ctx: &Invocation<'_>, args: IndexArgs) -> Result<Value> {
    // `index --preview` is read-only all the way through opening the repository:
    // the ordinary open path may repair a missing HEAD, which would violate the
    // preview no-state-creation contract before scanning.
    let repo = open_existing_managed_root(ctx.cwd(), ctx.control)?.in_lock_session(ctx.locks);
    run_index_for_repo(ctx, args, &repo, true)
}

/// Index one scope. Only the user-invoked parent discovers children; child
/// scopes stay direct-file-only and cannot recursively fan out a single run.
fn run_index_for_repo(
    ctx: &Invocation<'_>,
    args: IndexArgs,
    repo: &Repository,
    discover_children: bool,
) -> Result<Value> {
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
) -> Result<Value> {
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
                return Ok(gc::deferred_before_publication("index", report));
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
        let mut output = index_preview_json(repo.canonical_root(), &preview);
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
                let mut row = serde_json::to_value(&child)
                    .unwrap_or_else(|_| json!({ "path": child.path, "status": child.status }));
                if child.status == "planned" {
                    let Some(bound) =
                        bind_planned_child(plan, &child.path).map_err(pipeline_to_kio)?
                    else {
                        row["status"] = json!("skipped_vcs");
                        row["reason"] = json!("vcs_marker_added_after_discovery");
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
                    row["estimated_file_count"] = json!(count);
                    row["estimated_size_bytes"] = json!(bytes);
                    row["estimated_cost_usd"] = json!(cost);
                    aggregate_files += count;
                    aggregate_bytes += bytes;
                    aggregate_cost += cost;
                }
                children.push(row);
            }
            output["child_scopes"] = json!(children);
            output["estimated_aggregate_file_count"] = json!(aggregate_files);
            output["estimated_aggregate_size_bytes"] = json!(aggregate_bytes);
            output["estimated_aggregate_cost_usd"] = json!(aggregate_cost);
            output["child_scope_discovery"] = json!({
                "page_size": CHILD_SCOPE_DISCOVERY_PAGE_SIZE,
                "has_more": discovery.slice.continuation.is_some(),
                "preview": true,
            });
        }
        admission_policy.revalidate().map_err(pipeline_to_kio)?;
        return Ok(output);
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
    let mut output = json!({
        "status": if outcome.noop { "noop" } else { "indexed" },
        "approval_method": if args.yes { "yes" } else { "existing" },
        "network_allowed": index_result.network_allowed,
        "network_opt_in": persistent_network_allowed(adapter_settings, repo)?,
        "pending_online_tasks": index_result.pending_online_tasks,
        // R11-2: fold enrichment's budget pauses in with the enqueue-paused markdownize
        // tasks so `paused_tasks` reflects everything the run left paused.
        "paused_tasks": index_result.paused_tasks + enrichment.paused,
        "failed_files": index_result.failed_files,
        "normalized_files": index_result.normalized_files,
        "pending_files": index_result.pending_files,
        // R12-2: files skipped for adapter processing by the max_input_bytes gate.
        "skipped_oversized_files": index_result.skipped_oversized_files,
        // R22-4: archived binaries that no local pass and no online OCR can enrich.
        "skipped_unrecognized_binary_files": index_result.skipped_unrecognized_binary_files,
        // R11-2: disclose the inline embedding enrichment (was entirely absent, so an
        // auth/rate failure during index was invisible in the result JSON).
        "embedding_tasks_executed": enrichment.executed,
        "embedding_tasks_failed": enrichment.failed,
        "tree_hash": outcome.tree_hash,
        "commit_hash": outcome.commit_hash,
        "commit": outcome.commit,
        // F5: non-blocking budget warning (null unless a cap crossed warn_at_percent).
        "budget_warning": scope_budget_warning(repo)?,
    });
    // R16-4: disclose any documents skipped for missing/corrupt units (empty on a
    // clean index — the units were just written, so this normally stays empty).
    attach_skipped_units(&mut output, &rebuild_report, repo.kio_dir());
    // R11-3: an index that partially failed to normalize local files keeps its full
    // result JSON on stdout (commit_hash, tree_hash, …) with `error_code` +
    // `__exit_code:3`, matching search's "result + nonzero exit" shape — instead of
    // the old Err envelope that buried commit_hash inside a private `context.output`.
    if index_result.failed_files > 0 {
        output["error_code"] = json!("KIO-E-INDEX-PARTIAL-001");
    }
    // Exit priority (docs/04 §5.6 / §7): enrichment auth (5) > budget pause (6) >
    // local partial (3). All still print the full result JSON to stdout.
    let exit_override = enrichment_exit_override(&enrichment)
        .or_else(|| (index_result.failed_files > 0).then_some(ExitCode::PartialFailure));
    if let Some(code) = exit_override {
        set_exit_override(&mut output, code);
    }
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
        let mut child_gc_reports = BTreeMap::<String, Value>::new();
        let parent_exit_override = peek_exit_override(&output);
        let mut child_exit_overrides = Vec::new();
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
                        if peek_exit_override(&child_output).is_none() =>
                    {
                        cache_managed_child_parent(
                            &mut managed_parents,
                            child.path.clone(),
                            *child_repo,
                        );
                        if let Some(gc) = child_output.get("gc") {
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
                        if let Some(gc) = child_output.get("gc") {
                            child_gc_reports.insert(child.path.clone(), gc.clone());
                        }
                        let child_exit = peek_exit_override(&child_output)
                            .expect("guarded child output must carry a supported exit override");
                        child_exit_overrides.push(child_exit);
                        child.status = "indexed_partial".to_owned();
                        child.error_code = child_output
                            .get("error_code")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                            .or_else(|| Some(exit_override_error_code(child_exit).to_owned()));
                        child.message = child_output
                            .get("message")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                            .or_else(|| {
                                Some("child index completed with a non-success exit".to_owned())
                            });
                    }
                    Err(error) => {
                        incomplete_child_discovery = true;
                        page_completed = false;
                        if matches!(
                            error.exit_code(),
                            ExitCode::AuthError
                                | ExitCode::BudgetExceeded
                                | ExitCode::PartialFailure
                                | ExitCode::PermanentFailure
                        ) {
                            child_exit_overrides.push(error.exit_code());
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
        let mut child_values = serde_json::to_value(children).unwrap_or_else(|_| json!([]));
        if let Some(rows) = child_values.as_array_mut() {
            for row in rows {
                let Some(path) = row.get("path").and_then(Value::as_str) else {
                    continue;
                };
                if let Some(gc) = child_gc_reports.get(path) {
                    row["gc"] = gc.clone();
                }
            }
        }
        output["child_scopes"] = child_values;
        output["child_scope_discovery"] = json!({
            "page_size": CHILD_SCOPE_DISCOVERY_PAGE_SIZE,
            "has_more": has_more,
            "continuation_committed": continuation_committed,
            "restarted_from_root": restart_count > 0,
            "rows_total": child_rows_total,
            "rows_omitted": child_rows_omitted,
        });
        if let Some(code) = merged_index_exit_override(
            parent_exit_override,
            child_exit_overrides,
            incomplete_child_discovery,
        ) {
            if output.get("error_code").is_none() {
                output["error_code"] = json!(match code {
                    ExitCode::AuthError => "KIO-E-ADAPTER-AUTH-001",
                    ExitCode::BudgetExceeded => "KIO-E-BUDGET-EXCEEDED-001",
                    _ => "KIO-E-INDEX-PARTIAL-001",
                });
            }
            set_exit_override(&mut output, code);
        }
    }
    let successful_publication = peek_exit_override(&output).is_none();
    if !successful_publication && recovered_before_publication {
        output["gc"] = json!({
            "status": "completed",
            "trigger": "preflight_recovery",
            "mode": "after_index",
            "recovered_before_publication": true,
        });
    }
    // Automatic GC acquires its own descriptor-bound lock after publication.
    if successful_publication {
        let gc_binding = gc_binding.expect("successful index has automatic GC preflight binding");
        Ok(gc::attach_after_success(
            repo,
            "index",
            recovered_before_publication,
            gc_binding,
            output,
        ))
    } else {
        Ok(output)
    }
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

fn read_repository_scan_input(
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

fn repository_scan_policy_allows_file(
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
fn repository_input_display_path(repo: &Repository, input_path: &str) -> PathBuf {
    repo.canonical_root().join(input_path)
}

/// Execute one planned child against the retained discovery handle.  Its
/// inherited parent rules are kept only in the invocation-local policy stack.
enum BoundChildIndex {
    SkippedVcs,
    ExcludedIndependentRoot,
    ExcludedByPolicy,
    Output(Value, Box<Repository>),
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
            Ok(BoundChildIndex::Output(output, repo))
        }
    }
}

/// Aggregate `index_status` (05 §1.7, CT3-OBS-001) over the searched scopes.
/// `enriched_ratio` = done AI-enrichment tasks / all AI-enrichment tasks pooled
/// across scopes (count-weighted average). Enrichment tasks are Markdownize +
/// Embedding; "done" is `Done`/`Partial`. `budget_paused` is a budget-paused task or
/// an exhausted monthly budget (R22-7 — a secrets hold pauses work but not for budget).
fn compute_index_status(
    searched: &[SearchedScopeInfo],
    budget_ledger: Option<&LedgerReadSnapshot>,
    ledger_path: &Path,
    budget_month: &str,
    control: &ControlHandle,
    locks: &kio_core::scope::LockSession,
) -> Result<Value> {
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
                task_errors.push(json!({
                    "scope_path": scope.scope_path,
                    "error_code": error.error_code(),
                    "message": error.message(),
                }));
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
                            json!({
                                "scope_path": scope.scope_path,
                                "path": disposition.path,
                                "raw_hash": disposition.raw_hash,
                                "media_type": disposition.media_type,
                                "size_bytes": disposition.size_bytes,
                                "reason": disposition.reason,
                            })
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
                                json!({
                                    "scope_path": scope.scope_path,
                                    "path": disposition.path,
                                    "raw_hash": disposition.raw_hash,
                                    "media_type": disposition.media_type,
                                    "size_bytes": disposition.size_bytes,
                                    "reason": disposition.reason,
                                })
                            }),
                        );
                    }
                    Err(error) => unsupported_input_errors.push(json!({
                        "scope_path": scope.scope_path,
                        "error_code": error.error_code(),
                        "message": error.message(),
                    })),
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
                let error = json!({
                    "scope_path": scope.scope_path,
                    "error_code": error.error_code(),
                    "message": error.message(),
                });
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
    Ok(json!({
        "enriched_ratio": enriched_ratio,
        "pending_enrichment_tasks": pending,
        "budget_paused": budget_paused,
        "unsupported_inputs": unsupported_inputs,
        "unsupported_input_errors": unsupported_input_errors,
        "unsupported_inputs_complete": unsupported_inputs_complete,
        "task_errors": task_errors,
        "tasks_complete": tasks_complete,
        // 07 §5.1: Office (DOCX/PPTX) files skipped for online enrichment
        // because no converter (`KIO_OFFICE_CONVERTER` / PATH `soffice`)
        // resolved at `kio index` time -- no doomed task was enqueued for
        // them (item 1's enqueue gate). Disappears once a converter is
        // installed and the file is re-indexed (the disposition transitions
        // to `resolved`, dropping it from this "still live" view).
        "office_conversion_unavailable": {
            "count": office_conversion_unavailable_inputs.len(),
            "files": office_conversion_unavailable_inputs,
        },
    }))
}

fn current_unsupported_inputs(repo: &Repository) -> Result<Vec<UnsupportedInputDisposition>> {
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

fn record_unsupported_if_changed(
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

fn run_reindex(ctx: &Invocation<'_>, args: ReindexArgs) -> Result<Value> {
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
    let mut reindex_skipped = Vec::<Value>::new();
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
                reindex_skipped.push(json!({
                    "raw_hash": entry.raw_hash,
                    "path": entry.path,
                    "gen": normalize.r#gen,
                    "reason": error.error_code(),
                }));
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
    // `attach_skipped_units` disclosure below covers both phases. Dedup by raw_hash: a
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
    let mut output = json!({
        "status": "reindexed",
        "reindexed_files": reindexed,
        "commit_hash": outcome.commit_hash,
        "rebuilt_chunks": report.rebuilt_chunks,
        "embedding_tasks_executed": enrichment.executed,
        "embedding_tasks_failed": enrichment.failed,
        "paused_tasks": enrichment.paused,
    });
    // R16-4: disclose any documents skipped for missing/corrupt units.
    attach_skipped_units(&mut output, &report, repo.kio_dir());
    if let Some(code) = enrichment_exit_override(&enrichment) {
        set_exit_override(&mut output, code);
    }
    Ok(output)
}

fn atomic_overwrite_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| KioError::io("path has no parent", path.display().to_string()))?;
    fs::create_dir_all(parent)
        .map_err(|err| KioError::io(err.to_string(), parent.display().to_string()))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let tmp = parent.join(format!(
        ".{name}.tmp-{}-{}",
        process::id(),
        new_ulid(parent)
    ));
    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if let Err(err) = result {
        let _ = fs::remove_file(&tmp);
        return Err(KioError::io(err.to_string(), path.display().to_string()));
    }
    Ok(())
}

fn copy_normalized_instance_gen(
    kio_dir: &Path,
    raw_hash: &str,
    tool_profile_hash: &str,
    old_gen: u64,
    new_gen: u64,
    control: &ControlHandle,
) -> Result<()> {
    let old_dir = kio_pipeline::markdownize::normalized_instance_dir(
        kio_dir,
        raw_hash,
        tool_profile_hash,
        old_gen,
    );
    if let Ok(entries) = fs::read_dir(&old_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_str().is_some_and(is_orphan_temp_name)
                && entry
                    .file_type()
                    .map(|file_type| file_type.is_file())
                    .unwrap_or(false)
            {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    let mut instance = load_validated_normalized_instance_with_control(
        kio_dir,
        raw_hash,
        tool_profile_hash,
        old_gen,
        control,
    )
    .map_err(pipeline_to_kio)?;
    let generated_at = control.now_utc_seconds();
    instance.manifest.parent_gen = Some(old_gen);
    instance.manifest.r#gen = new_gen;
    instance.manifest.run_id = format!("run_{}", new_ulid(kio_dir));
    instance.manifest.generated_at = generated_at.clone();
    for unit in &mut instance.units {
        unit.r#gen = new_gen;
        unit.generated_at.clone_from(&generated_at);
    }
    persist_normalized_instance_with_control(kio_dir, &instance.manifest, &instance.units, control)
        .map(|_| ())
        .map_err(pipeline_to_kio)
}

/// Whether `name` matches the normalized-unit file naming convention written by
/// `persist_normalized_instance`: a 16-hex-char `unit_ref` digest plus `.json`
/// (e.g. `1a2b3c4d5e6f7089.json`). Anything else in a gen dir is not store truth
/// (R9-5) — `manifest.json`, a crashed writer's `.tmp-*`, a `.DS_Store`.
#[cfg(test)]
fn is_normalized_unit_file(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".json") else {
        return false;
    };
    stem.len() == 16
        && stem
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// Whether `name` looks like one of Kio's own atomic-write temp files
/// (`.tmp-<pid>-...` or `.<name>.tmp-<pid>-...`). Used by R9-5 to GC a
/// crash-orphaned temp left inside a gen dir by a killed writer.
fn is_orphan_temp_name(name: &str) -> bool {
    name.starts_with('.') && name.contains(".tmp-")
}

fn index_to_kio(error: kio_index::IndexError) -> KioError {
    KioError::schema(error.to_string())
}

fn search_to_kio(error: kio_search::SearchError) -> KioError {
    match error {
        kio_search::SearchError::Cursor(message) => KioError::new(
            "KIO-E-SEARCH-CURSOR-001",
            message,
            json!({}),
            ExitCode::InvalidUsage,
        ),
        kio_search::SearchError::Evidence(message) => KioError::schema(message),
        kio_search::SearchError::Contract(message) => KioError::schema(message),
    }
}

fn run_batch(ctx: &Invocation<'_>, args: BatchArgs) -> Result<Value> {
    let adapter_settings = ctx.settings;

    validate_batch_args(&args)?;
    // Same lane contract as `run_index`: resume/retry drive real online sends,
    // so the invocation lane has to be resolved before any of them is planned.
    let lane_override = match &args.command {
        Some(BatchCommand::Resume(resume)) => LaneOverride::new(resume.realtime, resume.batch),
        Some(BatchCommand::Retry(retry)) => LaneOverride::new(retry.realtime, retry.batch),
        Some(BatchCommand::Abandon(_)) | None => LaneOverride::default(),
    };
    let repo = ctx.open_recovery_repository()?;
    resolve_invocation_lane(
        lane_override,
        Some(&repo.kio_dir().join("config.toml")),
        ctx.control,
    )?;
    // O3: `batch resume` / `batch retry` read-modify-write `tasks.jsonl` and drive
    // online sends, so hold the folder store lock end-to-end — the same guard M1
    // wired onto index/repair/reindex. Without it two concurrent `batch resume`
    // runs interleave the ledger and double-send held tasks. Reentrant with any
    // inner auto-snapshot; losers fail fast with KIO-E-STORE-LOCKED-001 (exit 3).
    let _lock = repo.lock_store_in_session(repo.lock_session())?;
    // Persisted recovery work is executable authority. Revalidate the repository's
    // current tool lock after acquiring the store lock and before reading or
    // mutating tasks, so resume/retry cannot bypass the same adapter policy used by
    // index and repair paths.
    require_repo_tool_lock(&repo)?;
    // Ledger-only mutations must not poll or collect unrelated provider work
    // before their own confirmation. They still run under the same repository
    // lock and current tool-lock validation as every other batch mutation.
    match &args.command {
        Some(BatchCommand::Retry(retry)) if retry.reset_violations.is_some() => {
            return run_batch_reset_violations(
                ctx,
                retry.reset_violations.as_deref().expect("validated above"),
                retry.yes,
            );
        }
        Some(BatchCommand::Retry(retry)) if retry.resend_unknown.is_some() => {
            return run_batch_resend_unknown(
                ctx,
                &repo,
                retry.resend_unknown.as_deref().expect("validated above"),
                retry.yes,
            );
        }
        Some(BatchCommand::Abandon(abandon)) => {
            return run_batch_abandon(ctx, &abandon.selector, abandon.yes);
        }
        _ => {}
    }
    // CL45/item 5: reconcile any stale `request_kind='sync'` cost-ledger.sqlite
    // rows left by a crashed prior run — applies uniformly to resume/retry/
    // abandon, all of which reach this point before their own dispatch.
    let batch_offline = matches!(&args.command, Some(BatchCommand::Resume(resume)) if resume.offline)
        || matches!(&args.command, Some(BatchCommand::Retry(retry)) if retry.offline);
    let store = TaskStore::new_with_control(repo.kio_dir(), repo.control().clone());
    if !batch_offline && let Some(ledger) = open_optional_ledger_db(ctx.control)? {
        recover_stale_sync_rows(&ledger, Some(&store), &scope_id(repo.kio_dir())?)?;
    }
    // LC39/LC40 (see `ensure_purge_epoch_initialized`'s doc comment).
    ensure_purge_epoch_initialized(repo.kio_dir(), repo.control())?;
    // 04 §5.8 相 3: poll/collect this scope's in-flight Batch markdownize jobs
    // BEFORE dispatch, so a Success job lands its task Done ahead of the
    // Resume/Retry pass below (whose promotion step then publishes it), and a
    // still-queued job extends its poll schedule. Folded into the Resume/Retry
    // ExecOutcome so "in-flight のみ残" maps to the batch exit-3 semantics.
    let mut batch_poll_outcome = ExecOutcome::default();
    if !batch_offline && let Some(ledger) = open_optional_ledger_db(ctx.control)? {
        batch_poll_outcome = poll_batch_markdownize_jobs(adapter_settings, &repo, &store, &ledger)?;
        batch_poll_outcome.add(poll_batch_embedding_jobs(ctx, &repo, &ledger)?);
    }
    // N1: a Tier B online hold is only lifted by an explicit `--send-secrets`
    // approval, never by a plain `batch resume`. Without this, resume's
    // Paused -> Pending flip would silently un-hold the candidate-secret task.
    let online_profile = online_markdownize_profile_for(&repo)?;
    let secrets_approved = secrets_send_approved(
        adapter_settings,
        &repo,
        approvals::AdapterRole::Markdown,
        &online_profile.adapter_id,
        &online_profile.tool_profile_hash,
    );
    match args.command {
        Some(BatchCommand::Resume(resume)) => {
            // QA30: same mutual-exclusion validation `kio index`/`kio repair`
            // apply to their own `--online`/`--offline`.
            if resume.online && resume.offline {
                return Err(KioError::invalid_usage(
                    "--online and --offline are mutually exclusive",
                ));
            }
            // A recheck is an operator-requested state transition based on the
            // *current* cap. Validate that policy before unpausing anything: a
            // malformed replacement config must leave persisted budget holds
            // intact rather than turning them into Pending work which a later
            // invocation could send under an unknown policy.
            if resume.recheck_budget {
                read_budget_policy(
                    user_config_toml_path(ctx.control),
                    repo.kio_dir().join("config.toml"),
                )
                .map_err(pipeline_to_kio)?;
            }
            let changed = store
                .update_matching(|task| {
                    let held_secret = task.fallback_reason.as_deref() == Some(SECRETS_TIER_B_HOLD);
                    // QA2 (step4b-contract-tests-p3a.md §A, 04 §5.2): this generic
                    // flip already covers an auth hold (`hold_reason=auth`) — any
                    // non-budget, non-secret Paused task, auth included — so
                    // `batch resume` is the release for auth_error too, not just
                    // budget/tier_b.
                    if task.status == TaskStatus::Paused
                        && (resume.override_budget
                            || resume.recheck_budget
                            || task.fallback_reason.as_deref() != Some("budget_exceeded"))
                        && (!held_secret
                            || (secrets_approved
                                && targets_standard_online_markdownize(
                                    store.kio_dir(),
                                    task,
                                    &online_output_ref(&online_profile.adapter_id),
                                )))
                    {
                        task.status = TaskStatus::Pending;
                        task.fallback_reason = None;
                        task.hold_reason = None;
                        true
                    } else {
                        false
                    }
                })
                .map_err(pipeline_to_kio)?;
            let mut outcome = execute_pending_tasks(
                Invocation {
                    settings: adapter_settings,
                    context: ctx.context,
                    control: ctx.control,
                    locks: ctx.locks,
                },
                &repo,
                &store,
                resume.override_budget,
                true,
                resume.online,
                resume.offline,
            )?;
            outcome.add(batch_poll_outcome);
            let mut output = json!({
                "status": "resumed",
                "recheck_budget": resume.recheck_budget,
                "override_budget": resume.override_budget,
                "tasks_updated": changed,
                "tasks_executed": outcome.executed,
                // R9-7: surface driven online-send attempts and failures, not just
                // successes, so an orchestrator sees rate-limit / auth / charge
                // consumption even when nothing completed.
                "tasks_attempted": outcome.attempted(),
                "tasks_failed": outcome.failed,
                // R11-2: pauses this pass are a real state change (previously invisible
                // — `tasks_updated` reported 0 while the store flipped to paused).
                "tasks_paused": outcome.paused,
                // Batch lane (04 §5.8): jobs submitted or still queued/running
                // provider-side after this pass — completed by a later resume's poll.
                "tasks_inflight": outcome.inflight,
                // Subset of the above that is held because the provider's
                // response could not be READ. Non-zero = a fault to look at,
                // not a job to wait for.
                "tasks_inflight_unreadable": outcome.held_unreadable,
            });
            // R11-2: report the batch exit code (docs/04 §5.6) from what this pass did
            // — auth (5) / budget-paused (6) / partial (3) / all-permanent (4) — while
            // the full result JSON still prints to stdout. R14-5: with a batch-owned
            // error_code so errors.jsonl does not borrow a search code.
            apply_batch_exit_override(&mut output, &outcome);
            Ok(output)
        }
        Some(BatchCommand::Retry(retry_args)) => {
            // CL62-CL68/§M note-4: `--reset-violations <selector>` is a distinct
            // sub-mode — it targets exactly one cost-ledger.sqlite batch_requests
            // row (by intent_token or task-key selector, same form as `abandon`),
            // not the bulk tasks.jsonl retry scan below. Its dispatch returned
            // above so confirmation precedes provider polling.
            // QA30: same mutual-exclusion validation `kio index`/`kio repair`
            // apply to their own `--online`/`--offline`.
            if retry_args.online && retry_args.offline {
                return Err(KioError::invalid_usage(
                    "--online and --offline are mutually exclusive",
                ));
            }
            // R9-4: `batch retry` also recovers Partial online markdownize tasks
            // (docs/04 §5.2 `partial -> done`). A Partial task has some Done and
            // some Failed units; re-drive it (Pending) with `unit_keys` scoped to
            // the still-Failed units and the placeholder output_ref restored so
            // execute_pending_markdownize_tasks selects it. On full success it
            // becomes Done, ending the silent data gap index_status showed as
            // fully enriched.
            let incomplete_reenqueued = reenqueue_incomplete_markdownize_tasks(&store)?;
            let changed = store
                .update_matching(|task| {
                    if task.status == TaskStatus::Failed
                        // A validated online response with failed units has an immutable
                        // normalized manifest. Its unit-level retry plan owns recovery;
                        // the generic task-error retry must not erase that distinction.
                        && task.fallback_reason.as_deref() != Some("online_adapter_done")
                        && task_retry_allowed(task)
                        && task_retry_due(task, ctx.control)
                    {
                        task.status = TaskStatus::Pending;
                        task.next_retry_at = None;
                        true
                    } else {
                        false
                    }
                })
                .map_err(pipeline_to_kio)?;
            // `batch retry` never overrides the budget cap (retry re-schedules,
            // it does not bypass caps — `batch resume --override-budget` does), and never
            // revives an auth_error task (CT2-TASK-005: `max_attempts=0` is this command's
            // contract; `batch resume` is where repaired credentials take effect).
            let mut outcome = execute_pending_tasks(
                Invocation {
                    settings: adapter_settings,
                    context: ctx.context,
                    control: ctx.control,
                    locks: ctx.locks,
                },
                &repo,
                &store,
                false,
                false,
                retry_args.online,
                retry_args.offline,
            )?;
            outcome.add(batch_poll_outcome);
            let mut output = json!({
                "status": "retry scheduled",
                "tasks_updated": changed + incomplete_reenqueued,
                "tasks_executed": outcome.executed,
                // R9-7: surface driven online-send attempts and failures.
                "tasks_attempted": outcome.attempted(),
                "tasks_failed": outcome.failed,
                // R11-2: pause transitions this pass (docs/04 §5.6 visibility).
                "tasks_paused": outcome.paused,
                // Batch lane (04 §5.8): see the Resume arm's `tasks_inflight`.
                "tasks_inflight": outcome.inflight,
                // Subset of the above that is held because the provider's
                // response could not be READ. Non-zero = a fault to look at,
                // not a job to wait for.
                "tasks_inflight_unreadable": outcome.held_unreadable,
            });
            // R14-5: batch-owned error_code on the Partial(3)/Permanent(4) override.
            apply_batch_exit_override(&mut output, &outcome);
            Ok(output)
        }
        Some(BatchCommand::Abandon(_)) => unreachable!("handled before polling"),
        None => Err(KioError::not_implemented("batch command")),
    }
}

/// Enforce batch sub-mode constraints for direct app callers as well as Clap.
/// In particular, reset is a ledger mutation, so its transport/lane flags are
/// never silently ignored.
fn validate_batch_args(args: &BatchArgs) -> Result<()> {
    let Some(BatchCommand::Retry(retry)) = args.command.as_ref() else {
        return Ok(());
    };

    let ledger_mutation = retry.reset_violations.is_some() || retry.resend_unknown.is_some();
    if retry.reset_violations.is_some() && retry.resend_unknown.is_some() {
        return Err(KioError::invalid_usage(
            "--reset-violations and --resend-unknown are mutually exclusive",
        ));
    }
    if retry.yes && !ledger_mutation {
        return Err(KioError::invalid_usage(
            "--yes is valid only with batch retry --reset-violations or --resend-unknown",
        ));
    }
    if ledger_mutation && !retry.yes {
        return Err(KioError::invalid_usage(
            "--reset-violations and --resend-unknown require --yes",
        ));
    }
    if ledger_mutation && (retry.online || retry.offline || retry.realtime || retry.batch) {
        return Err(KioError::invalid_usage(
            "--reset-violations and --resend-unknown cannot be combined with --online, --offline, --realtime, or --batch",
        ));
    }
    Ok(())
}

fn run_adapter(ctx: &Invocation<'_>, args: AdapterArgs) -> Result<Value> {
    match args.command {
        Some(AdapterCommand::Approve(args)) => {
            approvals::approve(ctx, args, &approvals::ApprovalControl::default())
        }
        Some(AdapterCommand::Status(args)) => approvals::status(ctx, args),
        command => run_adapter_lifecycle(ctx.context, command, ctx.control),
    }
}

fn run_adapter_lifecycle(
    ctx: &AppContext,
    command: Option<AdapterCommand>,
    control: &ControlHandle,
) -> Result<Value> {
    match command {
        Some(AdapterCommand::Revoke(args)) => approvals::revoke(ctx, args, control),
        Some(AdapterCommand::Trust(command)) => {
            trust::run(&trust::directory_path(control), command, control)
        }
        None => Err(KioError::invalid_usage(
            "adapter requires approve, revoke, status, or trust",
        )),
        _ => unreachable!("content adapter commands require runtime settings"),
    }
}

fn run_ledger(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    args: LedgerArgs,
    control: &ControlHandle,
) -> Result<Value> {
    match args.command {
        Some(LedgerCommand::Reconcile(_)) => run_ledger_reconcile(adapter_settings, control),
        command => run_ledger_lifecycle(LedgerArgs { command }, control),
    }
}

fn run_ledger_lifecycle(args: LedgerArgs, control: &ControlHandle) -> Result<Value> {
    match args.command {
        Some(LedgerCommand::Init { resume }) => {
            let path = ledger_db_path(control);
            let _ledger = if resume {
                LedgerDb::resume_initialization_with_control(&path, control.clone())
            } else {
                LedgerDb::initialize_with_control(&path, control.clone())
            }
            .map_err(pipeline_to_kio)?;
            Ok(json!({ "status": "initialized", "path": path }))
        }
        Some(LedgerCommand::Status) => {
            match open_search_budget_snapshot(&ledger_db_path(control), control) {
                Ok(Some(_ledger)) => {
                    Ok(json!({ "status": "ready", "path": ledger_db_path(control) }))
                }
                Ok(None) => {
                    Ok(json!({ "status": "uninitialized", "path": ledger_db_path(control) }))
                }
                Err(error) => Err(error),
            }
        }
        Some(LedgerCommand::Backup(args)) => {
            if !args.to.is_absolute() {
                return Err(KioError::invalid_usage(
                    "ledger backup --to must be an absolute directory",
                ));
            }
            kio_core::private_fs::verify_private_directory_with_control(&args.to, control)?;
            let ledger = open_ledger_db(control)?;
            let backup = ledger.backup().map_err(pipeline_to_kio)?;
            backup
                .publish_create_only_with_control(&args.to, control)
                .map_err(pipeline_to_kio)?;
            Ok(json!({
                "status": "backed_up",
                "path": args.to,
            }))
        }
        Some(LedgerCommand::Restore(args)) => {
            if !args.from.is_absolute() {
                return Err(KioError::invalid_usage(
                    "ledger restore --from must be an absolute directory",
                ));
            }
            let path = ledger_db_path(control);
            let backup =
                kio_pipeline::ledger::LedgerBackup::load_with_control(&args.from, control.clone())
                    .map_err(pipeline_to_kio)?;
            let _ledger =
                LedgerDb::restore_missing_database_with_control(&path, &backup, control.clone())
                    .map_err(pipeline_to_kio)?;
            Ok(json!({ "status": "restored", "path": path }))
        }
        Some(LedgerCommand::Recover) => {
            let path = ledger_db_path(control);
            let recovery = LedgerDb::recover_pending_write_with_control(&path, control.clone())
                .map_err(pipeline_to_kio)?;
            Ok(json!({ "status": "ready", "recovery": recovery, "path": path }))
        }
        None => Err(KioError::invalid_usage(
            "ledger requires init, status, backup, restore, recover, or reconcile",
        )),
        Some(LedgerCommand::Reconcile(_)) => unreachable!("reconcile requires runtime settings"),
    }
}

/// `kio ledger reconcile` (QA14/QA15, step4b-contract-tests-p3a.md L307-338,
/// 10-operations.md §7.5.2 / 04-pipeline.md §5.8). Unlike `kio batch *`/
/// `kio adapter *`, this command is device-global — it does not operate on
/// the current directory's `.kio` scope (there may not even be one), so it
/// takes no `Repository::open_current()` and holds no `.kio/.lock` (the same
/// device-global-read posture `stalled_batch_status_json` already has;
/// unlike that function, this one also writes, but every write it makes
/// funnels through the same CAS/savepoint-guarded primitives every other
/// ledger writer uses, so no additional locking is needed here). Runnable
/// whether or not a restore was ever detected — see `LedgerCommand::Reconcile`'s
/// doc comment: idempotent, and useful standalone.
fn run_ledger_reconcile(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    control: &ControlHandle,
) -> Result<Value> {
    let ledger = open_ledger_db(control)?;
    let conn = &ledger;

    // Design step (a): integrity check, run as reconcile's own first action
    // regardless of whether `LedgerDb::open` already ran one this process
    // (it only does so "on detection" of a restore — reconcile always
    // checks). Table/index shape is already verified by `open`
    // (`ensure_schema`), so it is not re-checked here.
    let integrity = conn.integrity_check().map_err(pipeline_to_kio)?;
    if integrity != "ok" {
        // The restore-reconcile marker (if any) is deliberately left in
        // place — reconcile did not complete, so the gate must keep refusing
        // new submissions until a clean run succeeds.
        return Err(pipeline_to_kio(kio_pipeline::PipelineError::corrupt(
            "cost-ledger.sqlite",
            format!(
                "PRAGMA integrity_check reported corruption \
                 (10-operations.md §7.5.2): {integrity}"
            ),
        )));
    }

    // Design step (b): the existing sync-row crash recovery
    // (`recover_stale_sync_rows`), swept across every scope_id that carries
    // a sync row — this device-global command has no single owning scope to
    // scope that sweep to (see `distinct_scope_ids_with_sync_rows`'s doc
    // comment).
    let sync_scope_ids = kio_pipeline::ledger::ops::distinct_scope_ids_with_sync_rows(conn)
        .map_err(pipeline_to_kio)?;
    let mut sync_settled = 0_u64;
    for scope_id in &sync_scope_ids {
        sync_settled += recover_stale_sync_rows(&ledger, None, scope_id)?;
    }

    // The provider inventory drives both the batch-row recovery walk (design
    // step c: existing local rows -> does the provider have this job?) and
    // the orphan/unknown-attribution walk (steps d+e: provider jobs/uploads
    // -> is there a local row for this?) — fetched once, shared by both.
    // Batch send lane (07 §5.5 / 10 §7.5.2): `configured_inventories` itself
    // consults the configured Batch client (mock seam or the real Mistral
    // lane) and maps job metadata's 5-key contract onto these attribution
    // records — no CLI-side merge needed.
    let inventories = kio_adapter::batch_inventory::configured_inventories_with_control(
        adapter_settings,
        || batch_recovery_context(&ledger),
        control,
    )
    .map_err(adapter_to_kio)?;

    let batch_walk = run_batch_recovery_walk(conn, &inventories)?;
    let orphan_walk = run_orphan_attribution_walk(conn, &inventories)?;

    // Provider inventory can reconcile known intents; it cannot prove that
    // billing history lost from an older database never existed. This path
    // must never clear a history-loss gate or lower the durable checkpoint.

    Ok(json!({
        // `print_output`'s non-JSON fallback renders a one-line summary for
        // any payload carrying a `status` string (06-cli-spec.md §4's own
        // "human output = short summary" convention every other command
        // here already follows).
        "status": "reconciled",
        "integrity": integrity,
        "sync_settled": sync_settled,
        "batch_found": batch_walk.found,
        "batch_found_keys": batch_walk.found_keys,
        "batch_settled_unknown": batch_walk.settled_unknown,
        "batch_settled_unknown_keys": batch_walk.settled_unknown_keys,
        "unlistable": batch_walk.unlistable,
        "unlistable_keys": batch_walk.unlistable_keys,
        // Additive beyond the QA14 design's minimum JSON shape (04 §5.8
        // L1063-1066's "相 2b 未着手行は job 一覧照合の対象外" rows — reported,
        // not silently dropped, since no upload-cleanup machinery exists yet
        // to do anything else with them).
        "phase2b_not_started": batch_walk.not_job_matchable,
        "phase2b_not_started_keys": batch_walk.not_job_matchable_keys,
        "orphans": orphan_walk.orphans,
        "unknown": orphan_walk.unknown,
        "unknown_uploads": orphan_walk.unknown_uploads,
    }))
}

/// `scope_id/adapter_kind/input_hash/tool_profile_hash` — the same 4-tuple
/// selector shape `parse_batch_selector` accepts, used here only for
/// human/machine-readable reporting (never re-parsed).
fn task_key_selector_string(key: &LedgerTaskKey) -> String {
    format!(
        "{}/{}/{}/{}",
        key.scope_id, key.adapter_kind, key.input_hash, key.tool_profile_hash
    )
}

/// QA15 design step (c) — `kio ledger reconcile`'s batch-row recovery walk:
/// the FIRST production wiring of `recovery_candidates` (previously
/// test-only, per this session's recon — see the implementation report). For
/// each candidate row: an inventory covering its `provider_scope_id` that
/// lists a job matching the row's `intent_token` -> `found` (self-describes
/// `batch_job_id` via `recovery_mark_found` if not already recorded); no
/// match, but the recovery deadline AND visibility grace period have both
/// elapsed (04 §5.8 confirmed-absent) -> `unknown_settled`
/// (`recovery_settle_unknown`); a row whose `provider_scope_id` has no
/// configured inventory at all -> `unlistable` (10 §7.5.2: "どちらにも無い
/// scope は原理的に走査できない" — no state change, retried on a later run).
/// Rows with `provider_scope_id IS NULL` (相 2b 未着手) are excluded from
/// job-matching entirely per 04 §5.8 L1063-1066 ("job 不存在は記録から確定
/// している") and reported separately (`not_job_matchable`) — no
/// upload-cleanup machinery exists yet for them, so reconcile reports them
/// rather than attempting anything.
struct BatchRecoveryWalkReport {
    found: u64,
    found_keys: Vec<String>,
    settled_unknown: u64,
    settled_unknown_keys: Vec<String>,
    unlistable: u64,
    unlistable_keys: Vec<String>,
    not_job_matchable: u64,
    not_job_matchable_keys: Vec<String>,
}

fn run_batch_recovery_walk(
    conn: &LedgerDb,
    inventories: &[kio_adapter::batch_inventory::ProviderInventory],
) -> Result<BatchRecoveryWalkReport> {
    let now_ms = kio_pipeline::ledger::time::now_millis();
    let candidates =
        kio_pipeline::ledger::ops::recovery_candidates(conn).map_err(pipeline_to_kio)?;
    let mut report = BatchRecoveryWalkReport {
        found: 0,
        found_keys: Vec::new(),
        settled_unknown: 0,
        settled_unknown_keys: Vec::new(),
        unlistable: 0,
        unlistable_keys: Vec::new(),
        not_job_matchable: 0,
        not_job_matchable_keys: Vec::new(),
    };
    for row in candidates {
        let selector = task_key_selector_string(&row.key);
        let Some(provider_scope_id) = row.provider_scope_id.clone() else {
            report.not_job_matchable += 1;
            report.not_job_matchable_keys.push(selector);
            continue;
        };
        let Some(inventory) = inventories
            .iter()
            .find(|inventory| inventory.provider_scope_id == provider_scope_id)
        else {
            report.unlistable += 1;
            report.unlistable_keys.push(selector);
            continue;
        };
        let matched_job = row.intent_token.as_deref().and_then(|token| {
            inventory
                .jobs
                .iter()
                .find(|job| job.intent_token.as_deref() == Some(token))
        });
        if let Some(job) = matched_job {
            kio_pipeline::ledger::ops::recovery_mark_found(conn, &row.key, &job.job_id)
                .map_err(pipeline_to_kio)?;
            report.found += 1;
            report.found_keys.push(selector);
            continue;
        }
        let deadline_passed = kio_pipeline::ledger::ops::recovery_deadline_passed(
            &row,
            now_ms,
            kio_pipeline::ledger::ops::DEFAULT_RECOVERY_DEADLINE_MS,
        );
        let grace_elapsed = kio_pipeline::ledger::ops::visibility_grace_period_elapsed(
            row.job_create_started_at,
            now_ms,
            kio_pipeline::ledger::ops::DEFAULT_VISIBILITY_GRACE_PERIOD_MS,
        );
        if deadline_passed
            && grace_elapsed
            && let Some(token) = row.intent_token.clone()
        {
            // `provider_scope_id` is `Some` in this branch by
            // construction (the early-continue guard above), so upload
            // cleanup can never already be confirmed complete here —
            // mirrors `execute_abandon`'s own
            // `row.provider_scope_id.is_none()` rule for the same
            // `cleanup_already_complete` argument.
            recovery_settle_unknown(conn, &row.key, &token, row.estimated_usd, false)
                .map_err(pipeline_to_kio)?;
            report.settled_unknown += 1;
            report.settled_unknown_keys.push(selector);
        }
        // else: neither found nor confirmed-absent yet — 04 §5.8's "unknown"
        // bullet ("何も変更せず保持し、次回再試行する"): leave untouched,
        // retried on a later `kio ledger reconcile` run.
    }
    Ok(report)
}

/// 10 §7.5.2 L660-663's "ローカル構成の scope" set: every `scope_registry` row
/// whose `root_path/.kio/scope.json` can be read AND whose `scope_id` equals
/// the registry row's own — "実地検証 (読取 + scope_id 一致) できた scope_id
/// 集合". An unreadable or mismatched row is excluded (its jobs fall to
/// `unknown`, not `orphans` — 10 §7.5.2: "未再登録 scope の job は unknown 側
/// に落ち、再登録後の再実行で orphan 候補へ移る", which is exactly what
/// happens the next time this function runs after re-registration — the
/// walk is idempotent and self-correcting, not a one-shot classification).
/// Registry-open/read failure degrades to an empty set (everything falls to
/// `unknown`) rather than failing the whole reconcile — the registry is a
/// recoverable search cache (03-data-model.md §4), never truth.
fn verified_local_scope_ids(control: &ControlHandle) -> BTreeSet<String> {
    let mut verified = BTreeSet::new();
    let Ok(registry) = RegistryDb::open_default_with_control(control) else {
        return verified;
    };
    let Ok(entries) = registry.all_entries() else {
        return verified;
    };
    for entry in entries {
        let kio_dir = PathBuf::from(&entry.root_path).join(".kio");
        if let Ok(found_scope_id) = scope_id(&kio_dir)
            && found_scope_id == entry.scope_id
        {
            verified.insert(entry.scope_id);
        }
    }
    verified
}

struct OrphanAttributionReport {
    orphans: Vec<Value>,
    unknown: Vec<Value>,
    unknown_uploads: Vec<Value>,
}

/// QA15 design steps (d)+(e) — `kio ledger reconcile`'s orphan/unknown-
/// attribution walk (10 §7.5.2): the OTHER direction of matching from
/// `run_batch_recovery_walk` ("does the provider have a job with no local
/// row", not "does my row have a matching provider job"). Read-only —
/// nothing here mutates `batch_requests`/`cost_ledger`, so rerunning
/// reconcile produces an identical report (10 §7.5.2: "報告は冪等"). Nothing
/// is auto-resubmitted or auto-deleted, matching 10 §7.5.2's explicit rule.
fn run_orphan_attribution_walk(
    conn: &LedgerDb,
    inventories: &[kio_adapter::batch_inventory::ProviderInventory],
) -> Result<OrphanAttributionReport> {
    let verified_scopes = verified_local_scope_ids(conn.control());
    let mut orphans = Vec::new();
    let mut unknown = Vec::new();
    let mut unknown_uploads = Vec::new();

    for inventory in inventories {
        for job in &inventory.jobs {
            if job_is_accounted_for(conn, job)? {
                continue;
            }
            match &job.task_key {
                Some(task_key) if verified_scopes.contains(&task_key.scope_id) => {
                    orphans.push(json!({
                        "job_id": job.job_id,
                        "task_key": {
                            "scope_id": task_key.scope_id,
                            "adapter_kind": task_key.adapter_kind,
                            "input_hash": task_key.input_hash,
                            "tool_profile_hash": task_key.tool_profile_hash,
                        },
                        "guidance": {
                            "result_fetch": "read-only-safe: fetching this job's result does not create new state",
                            "deletion": "delete only after confirming no other Kio instance shares this provider scope",
                        },
                    }));
                }
                Some(task_key) => {
                    unknown.push(json!({
                        "job_id": job.job_id,
                        "task_key": {
                            "scope_id": task_key.scope_id,
                            "adapter_kind": task_key.adapter_kind,
                            "input_hash": task_key.input_hash,
                            "tool_profile_hash": task_key.tool_profile_hash,
                        },
                        "reason": "task_key.scope_id is not a locally-verified registered scope",
                    }));
                }
                None => {
                    unknown.push(json!({
                        "job_id": job.job_id,
                        "task_key": Value::Null,
                        "reason": "job metadata carries no task_key (unreadable or absent)",
                    }));
                }
            }
        }
        for upload in &inventory.uploads {
            let accounted_for = match &upload.filename_token {
                Some(token) => matches!(
                    resolve_abandon_selector(conn, &AbandonSelector::IntentToken(token.clone()))
                        .map_err(pipeline_to_kio)?,
                    AbandonResolution::Found(_)
                ),
                None => false,
            };
            if !accounted_for {
                unknown_uploads.push(json!({
                    "upload_id": upload.upload_id,
                    "filename_token": upload.filename_token,
                }));
            }
        }
    }

    Ok(OrphanAttributionReport {
        orphans,
        unknown,
        unknown_uploads,
    })
}

fn provider_task_ledger_key(key: &kio_adapter::batch_inventory::ProviderTaskKey) -> LedgerTaskKey {
    LedgerTaskKey::new(
        key.scope_id.clone(),
        key.adapter_kind.clone(),
        key.input_hash.clone(),
        key.tool_profile_hash.clone(),
    )
}

/// Whether some local `batch_requests` row already accounts for `job` — by
/// its `task_key`'s exact 4-tuple PRIMARY KEY match first (10 §7.5.2's
/// primary attribution rule: "job の帰属は...job metadata の task key 4 組が
/// 担う"), then, secondarily, by `intent_token` (04 §5.8's found/
/// confirmed-absent matching key — a defense-in-depth check for a job whose
/// `task_key` this fixture/provider never populated but whose token still
/// resolves).
fn job_is_accounted_for(
    conn: &LedgerDb,
    job: &kio_adapter::batch_inventory::ProviderJobRecord,
) -> Result<bool> {
    if let Some(task_key) = &job.task_key {
        let key = provider_task_ledger_key(task_key);
        if get_batch_request(conn, &key)
            .map_err(pipeline_to_kio)?
            .is_some()
        {
            return Ok(true);
        }
    }
    if let Some(token) = &job.intent_token
        && let AbandonResolution::Found(_) =
            resolve_abandon_selector(conn, &AbandonSelector::IntentToken(token.clone()))
                .map_err(pipeline_to_kio)?
    {
        return Ok(true);
    }
    Ok(false)
}

/// `$XDG_DATA_HOME/kio/cost-ledger.sqlite` (04 §5.4 / CL70) — the device-global
/// ledger, sole store for every reservation/charge this CLI records.
fn ledger_db_path(control: &ControlHandle) -> PathBuf {
    data_home(control).join("kio/cost-ledger.sqlite")
}

/// Capture the device ledger once for a search response's budget-status
/// observation. A completely absent ledger and authority set is the one normal
/// uninitialized state; spending is unknown. Every unsafe or unstable state is a
/// command-level failure and must not be reported as `budget_paused=false`.
fn open_search_budget_snapshot(
    path: &Path,
    control: &ControlHandle,
) -> Result<Option<LedgerReadSnapshot>> {
    match LedgerReadSnapshot::open_with_control(path, control.clone()) {
        Ok(snapshot) => Ok(Some(snapshot)),
        Err(LedgerSnapshotError::Missing) => Ok(None),
        Err(error) => Err(pipeline_to_kio(
            kio_pipeline::PipelineError::ledger_snapshot(path.display().to_string(), error),
        )),
    }
}

/// Capture the ledger snapshot and the month it is allowed to describe as one
/// observation. A rollover while opening a snapshot would otherwise pair a
/// September clock read with an October snapshot (or the reverse), so retry the
/// whole observation once. Two consecutive rollover crossings are surfaced as
/// retryable partial work rather than looping around a changing wall clock.
fn capture_search_budget_observation(
    path: &Path,
    control: &ControlHandle,
) -> Result<(String, Option<LedgerReadSnapshot>)> {
    capture_search_budget_observation_with(path, || control.now_utc_seconds(), control)
}

fn capture_search_budget_observation_with<F>(
    path: &Path,
    mut clock: F,
    control: &ControlHandle,
) -> Result<(String, Option<LedgerReadSnapshot>)>
where
    F: FnMut() -> String,
{
    const MAX_ATTEMPTS: usize = 2;
    for _ in 0..MAX_ATTEMPTS {
        let before_month = utc_month(&clock());
        let snapshot = open_search_budget_snapshot(path, control)?;
        let after_month = utc_month(&clock());
        if before_month == after_month {
            return Ok((before_month, snapshot));
        }
    }
    Err(KioError::new(
        "KIO-E-LEDGER-SNAPSHOT-ROLLOVER-001",
        "cost ledger month changed while capturing a budget snapshot; retry the command",
        json!({ "ledger_path": path, "reason": "month_rollover" }),
        ExitCode::PartialFailure,
    ))
}

fn ledger_snapshot_to_kio(path: &str, error: LedgerSnapshotError) -> KioError {
    match error {
        // A successfully-owned snapshot cannot become Missing while querying.
        // Treat that impossible transition as integrity failure rather than
        // silently switching to the no-ledger/zero-total outcome.
        LedgerSnapshotError::Missing => KioError::new(
            "KIO-E-LEDGER-SNAPSHOT-UNSAFE-001",
            "cost ledger snapshot disappeared after it was captured",
            json!({
                "ledger_path": path,
                "reason": "unsafe_integrity",
                "detail": "owned ledger snapshot reported Missing after capture",
            }),
            ExitCode::PermanentFailure,
        ),
        LedgerSnapshotError::UnsafeIntegrity(detail) => KioError::new(
            "KIO-E-LEDGER-SNAPSHOT-UNSAFE-001",
            "cost ledger could not be read without violating integrity safeguards",
            json!({
                "ledger_path": path,
                "reason": "unsafe_integrity",
                "detail": detail,
            }),
            ExitCode::PermanentFailure,
        ),
        LedgerSnapshotError::UnstableBusy(detail) => KioError::new(
            "KIO-E-LEDGER-SNAPSHOT-001",
            "cost ledger could not be read from a stable snapshot; retry the command",
            json!({
                "ledger_path": path,
                "reason": "unstable_or_busy",
                "detail": detail,
            }),
            ExitCode::PartialFailure,
        ),
    }
}

/// Open the device-global `cost-ledger.sqlite`. Every ledger-touching command (`kio status`,
/// `kio batch *`, `kio index`, `kio search --vector`/`--hybrid` page 1, `kio
/// purge`) opens the ledger through this one function, so every command uses
/// the current SQLite store. `LedgerDb::open` fails closed
/// if retired JSONL ledger files are present, leaving their bytes untouched.
fn open_ledger_db(control: &ControlHandle) -> Result<LedgerDb> {
    LedgerDb::open_existing_with_control(ledger_db_path(control), control.clone())
        .map_err(pipeline_to_kio)
}

/// Absence is optional only for callers that do not authorize paid work.
/// Partial, replaced or corrupt state always keeps its original error.
fn open_optional_ledger_db(control: &ControlHandle) -> Result<Option<LedgerDb>> {
    match LedgerDb::open_existing_with_control(ledger_db_path(control), control.clone()) {
        Ok(ledger) => Ok(Some(ledger)),
        Err(kio_pipeline::PipelineError::Contract {
            code: "KIO-E-LEDGER-UNINITIALIZED-001",
            ..
        }) => Ok(None),
        Err(error) => Err(pipeline_to_kio(error)),
    }
}

const SYNC_RESULT_UNKNOWN_LEDGER_ERROR: &str = "unknown_settled";
const SYNC_RESEND_AUTHORIZED_REASON: &str = "unknown_resend_authorized";

/// Mark exactly one task terminal after a durable sync unknown-outcome fence.
/// The ledger remains the crash-safe authority: if this JSONL update is
/// interrupted, `sync_result_unknown_fenced` repeats it before any send.
fn mark_sync_result_unknown_task(store: &TaskStore, task_id: &str) -> Result<bool> {
    store
        .update_matching(|task| {
            if task.task_id != task_id {
                return false;
            }
            task.status = TaskStatus::Failed;
            task.fallback_reason = Some(RESULT_UNKNOWN_REASON.to_owned());
            task.hold_reason = None;
            task.heartbeat_at = None;
            task.next_retry_at = None;
            task.attempts = task.attempts.saturating_add(1);
            task.clear_reservation();
            true
        })
        .map_err(pipeline_to_kio)
        .map(|changed| changed > 0)
}

/// A terminal sync `unknown_settled` row is a durable no-resend fence. The
/// explicit recovery command changes this exact error to its one-shot
/// authorization sentinel; every other terminal result follows its ordinary
/// lifecycle.
fn sync_result_unknown_fenced(row: Option<&BatchRequestRow>) -> bool {
    row.is_some_and(|row| {
        row.request_kind == RequestKind::Sync
            && row.state.is_terminal()
            && row.error.as_deref() == Some(SYNC_RESULT_UNKNOWN_LEDGER_ERROR)
    })
}

/// CL45/§5.4 sync crash recovery — the write-command-entry pass 04 §5.8's
/// batch recovery (CL32) runs alongside, but explicitly excludes
/// `request_kind='sync'` rows from (item 5 of this session's task: the
/// ledger-side settlement primitive, `recovery_settle_unknown`, is already
/// implemented and already reused for exactly this purpose by
/// `settle_task_charge_unknown` — this function is the write-entry caller
/// that was still missing).
///
/// No Adapter in this codebase exposes a post-hoc "query a past sync call's
/// result by provider request id" capability (confirmed by grep of
/// `kio-adapter`'s traits — the same fact `settle_task_charge_unknown`'s own
/// doc comment already records), so CL45's "batch_job_id 記録済みで照会可能"
/// branch is unreachable today: every stale sync row this finds settles via
/// the "未記録・照会不能" branch — `estimated_usd` billed with `estimated=1`,
/// `state=3`, `intent_token` cleared in the same Tx (CL47: sync rows have no
/// upload/job residue to wait on, unlike batch rows).
///
/// Scoped to `scope_id` (a sync row belongs to one scope; only that scope's
/// `.kio/.lock` holder reconciles it) and gated on `stale_after_at`
/// (`sync_recovery_candidates`'s own doc comment) so a genuinely live
/// concurrent sync call from another process is never raced.
fn recover_stale_sync_rows(
    ledger: &LedgerDb,
    store: Option<&TaskStore>,
    scope_id: &str,
) -> Result<u64> {
    let now_ms = kio_pipeline::ledger::time::now_millis();
    let candidates = sync_recovery_candidates(ledger, scope_id, now_ms).map_err(pipeline_to_kio)?;
    let mut settled = 0_u64;
    for row in candidates {
        let Some(intent_token) = row.intent_token.as_deref() else {
            // A state 0/1 row always carries an intent_token by construction
            // (`phase1_intent` always sets one on the same INSERT/UPDATE that
            // sets state=0); skip defensively rather than panic if this
            // invariant is ever violated.
            continue;
        };
        recovery_settle_unknown(ledger, &row.key, intent_token, row.estimated_usd, true)
            .map_err(pipeline_to_kio)?;
        // `reservation_id` is the precise task-side binding to this intent.
        // If a crash follows settlement, the durable row fence above prevents
        // a reclaimed Pending task from being re-sent before this update replays.
        if let Some(store) = store {
            store
                .update_matching(|task| {
                    if task.reservation_id.as_deref() != Some(intent_token) {
                        return false;
                    }
                    task.status = TaskStatus::Failed;
                    task.fallback_reason = Some(RESULT_UNKNOWN_REASON.to_owned());
                    task.hold_reason = None;
                    task.heartbeat_at = None;
                    task.next_retry_at = None;
                    task.attempts = task.attempts.saturating_add(1);
                    task.clear_reservation();
                    true
                })
                .map_err(pipeline_to_kio)?;
        }
        settled += 1;
    }
    Ok(settled)
}

/// `kio batch abandon` / `--reset-violations`'s shared selector grammar (06
/// §1 L44/L26-30, CL62): an `intent_token` (no `/`), or a `scope_id/adapter_kind/
/// input_hash[/tool_profile_hash]` task-key path (3-tuple accepted; resolution
/// rejects it if ambiguous).
fn parse_batch_selector(input: &str) -> Result<AbandonSelector> {
    if !input.contains('/') {
        return Ok(AbandonSelector::IntentToken(input.to_owned()));
    }
    let parts: Vec<&str> = input.split('/').collect();
    match parts.as_slice() {
        [scope_id, adapter_kind, input_hash] => Ok(AbandonSelector::ThreeTuple {
            scope_id: (*scope_id).to_owned(),
            adapter_kind: (*adapter_kind).to_owned(),
            input_hash: (*input_hash).to_owned(),
        }),
        [scope_id, adapter_kind, input_hash, tool_profile_hash] => Ok(AbandonSelector::TaskKey(
            LedgerTaskKey::new(*scope_id, *adapter_kind, *input_hash, *tool_profile_hash),
        )),
        _ => Err(KioError::invalid_usage(
            "selector must be an intent_token or scope_id/adapter_kind/input_hash[/tool_profile_hash]",
        )),
    }
}

/// Batch mutations need explicit `--yes` in non-interactive mode. A piped
/// response is never an approval: it is not reviewable at the terminal.
fn confirm_batch_mutation(ctx: &Invocation<'_>, prompt: &str, yes: bool) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !ctx.interaction.is_interactive() {
        return Err(batch_confirmation_rejected(
            "batch mutation requires --yes in non-interactive mode",
        ));
    }
    if confirm_batch_action(ctx, &format!("{prompt} [y/N]"))? {
        Ok(())
    } else {
        Err(batch_confirmation_rejected(
            "batch mutation confirmation was rejected",
        ))
    }
}

/// Shared terminal prompt for commands whose individual handlers decide
/// whether non-interactive confirmation is permitted.
fn confirm_batch_action(ctx: &Invocation<'_>, prompt: &str) -> Result<bool> {
    ctx.interaction.confirm(prompt)
}

/// 06 §1 requires a confirmation prompt before `repair verify-objects
/// --prune-orphans` and `repair registry-prune` delete anything. Both first
/// run their walk in preview mode, so `count` is what WOULD be removed.
///
/// Nothing to remove is an idempotent success with no prompt — the same
/// "対象なしの冪等成功" convention `kio adapter revoke` uses.
/// How many targets the prompt spells out before collapsing the rest to a
/// count. Enough to make a small prune fully reviewable without turning a large
/// one into an unreadable wall.
const REPAIR_PRUNE_PROMPT_MAX_LINES: usize = 20;

/// 06 §1's confirmation for a destructive `repair` operation.
///
/// H2-3 (R24b, 3/3 系統一致): the spec says 削除対象を先に列挙して見せてから問う
/// — show WHAT will be deleted, then ask. This used to take only a count, so
/// the user approved a number and never saw the objects behind it.
fn confirm_repair_prune(
    ctx: &Invocation<'_>,
    what: &str,
    targets: &[String],
    yes: bool,
) -> Result<()> {
    let count = targets.len() as u64;
    if count == 0 || yes {
        return Ok(());
    }
    if !ctx.interaction.is_interactive() {
        return Err(KioError::new(
            "KIO-E-CONFIRM-REJECTED-001",
            format!("repair {what} requires confirmation; pass --yes in non-interactive mode"),
            json!({ "target_count": count }),
            ExitCode::ConfirmationRejected,
        ));
    }
    let mut preview = format!("repair {what} will permanently remove {count} item(s):\n");
    for line in targets.iter().take(REPAIR_PRUNE_PROMPT_MAX_LINES) {
        preview.push_str("  ");
        preview.push_str(line);
        preview.push('\n');
    }
    if let Some(rest) = count.checked_sub(REPAIR_PRUNE_PROMPT_MAX_LINES as u64)
        && rest > 0
    {
        preview.push_str(&format!("  … and {rest} more\n"));
    }
    preview.push_str("Proceed?");
    if confirm_batch_action(ctx, &preview)? {
        return Ok(());
    }
    Err(KioError::new(
        "KIO-E-CONFIRM-REJECTED-001",
        format!("repair {what} confirmation was rejected"),
        json!({ "target_count": count }),
        ExitCode::ConfirmationRejected,
    ))
}

fn batch_confirmation_rejected(message: &str) -> KioError {
    KioError::new(
        "KIO-E-BATCH-CONFIRMATION-REJECTED-001",
        message,
        json!({}),
        ExitCode::ConfirmationRejected,
    )
}

/// `kio batch abandon <selector>` (06 §1 / 04 §5.8 恒久 unknown 脱出路,
/// CL62-CL68). Runs inside `run_batch`'s already-acquired `.kio/.lock`.
fn run_batch_abandon(ctx: &Invocation<'_>, selector_input: &str, yes: bool) -> Result<Value> {
    let selector = parse_batch_selector(selector_input)?;
    let ledger = open_ledger_db(ctx.control)?;
    let resolution = resolve_abandon_selector(&ledger, &selector).map_err(pipeline_to_kio)?;
    match resolution {
        // CL66: no matching row (terminal-confirmed / device-row pruned /
        // already abandoned-and-cleaned / never existed) is an idempotent
        // exit-0 success — no confirmation prompt needed, nothing to abandon.
        AbandonResolution::NotFound => Ok(json!({ "status": "no_target" })),
        AbandonResolution::Ambiguous => Err(KioError::invalid_usage(
            "abandon selector is ambiguous (multiple tool_profile_hash rows match this \
             scope/adapter/input_hash) — specify the intent_token or the full 4-tuple",
        )),
        AbandonResolution::Found(key) => {
            confirm_batch_mutation(
                ctx,
                &format!(
                    "Abandon in-flight batch request for {}/{}/{}?",
                    key.scope_id, key.adapter_kind, key.input_hash
                ),
                yes,
            )?;
            let execution = execute_abandon(&ledger, &key).map_err(pipeline_to_kio)?;
            Ok(json!({
                "status": match execution {
                    AbandonExecution::NoTarget => "no_target",
                    AbandonExecution::Abandoned => "abandoned",
                },
                "scope_id": key.scope_id,
                "adapter_kind": key.adapter_kind,
                "input_hash": key.input_hash,
            }))
        }
    }
}

/// `kio batch retry --reset-violations <selector>` (06 §1 L26-30, §M note-6).
fn run_batch_reset_violations(
    ctx: &Invocation<'_>,
    selector_input: &str,
    yes: bool,
) -> Result<Value> {
    let selector = parse_batch_selector(selector_input)?;
    let ledger = open_ledger_db(ctx.control)?;
    let resolution = resolve_abandon_selector(&ledger, &selector).map_err(pipeline_to_kio)?;
    match resolution {
        AbandonResolution::NotFound => Ok(json!({ "status": "no_target" })),
        AbandonResolution::Ambiguous => Err(KioError::invalid_usage(
            "--reset-violations selector is ambiguous (multiple tool_profile_hash rows match \
             this scope/adapter/input_hash) — specify the intent_token or the full 4-tuple",
        )),
        AbandonResolution::Found(key) => {
            confirm_batch_mutation(
                ctx,
                &format!(
                    "Reset contract_violation_count for {}/{}/{}?",
                    key.scope_id, key.adapter_kind, key.input_hash
                ),
                yes,
            )?;
            // §M note-6: count==0 (or an in-flight state 0/1 row, excluded by
            // the SQL) is a no-op success; only a terminal, non-zero-count row
            // actually changes.
            let did_reset = reset_contract_violations(&ledger, &key).map_err(pipeline_to_kio)?;
            Ok(json!({
                "status": if did_reset { "reset" } else { "unchanged" },
                "scope_id": key.scope_id,
                "adapter_kind": key.adapter_kind,
                "input_hash": key.input_hash,
            }))
        }
    }
}

/// Explicitly authorize one fresh synchronous request after an unknown result.
/// This command never contacts an adapter: it only changes the ledger's
/// one-shot fence and requeues the exact current task for a later online pass.
fn run_batch_resend_unknown(
    ctx: &Invocation<'_>,
    repo: &Repository,
    selector_input: &str,
    yes: bool,
) -> Result<Value> {
    let adapter_settings = ctx.settings;

    let selector = parse_batch_selector(selector_input)?;
    let ledger = open_ledger_db(ctx.control)?;
    let resolution = resolve_abandon_selector(&ledger, &selector).map_err(pipeline_to_kio)?;
    let key = match resolution {
        AbandonResolution::Found(key) => key,
        other => {
            return match other {
                AbandonResolution::NotFound => Ok(json!({ "status": "no_target" })),
                AbandonResolution::Ambiguous => Err(KioError::invalid_usage(
                    "--resend-unknown selector is ambiguous; specify the intent_token or full 4-tuple",
                )),
                AbandonResolution::Found(_) => unreachable!(),
            };
        }
    };
    let current_scope_id = repo.scope_identity()?.scope_id;
    if key.scope_id != current_scope_id {
        return Err(KioError::invalid_usage(
            "--resend-unknown selector does not belong to the current repository scope",
        ));
    }
    let markdown_profile = online_markdownize_profile_for(repo)?;
    let embedding_profile =
        embedding_execution(adapter_settings, ctx.control).map(declared_embedding_profile);
    let store = TaskStore::new_with_control(repo.kio_dir(), repo.control().clone());
    let tasks = store.all().map_err(pipeline_to_kio)?;
    let matching_task_ids = match key.adapter_kind.as_str() {
        "markdownize" if key.tool_profile_hash == markdown_profile.tool_profile_hash => tasks
            .iter()
            .filter(|task| {
                task.task_type == TaskType::Markdownize
                    && task.input_hash == key.input_hash
                    && task.status == TaskStatus::Failed
                    && task.fallback_reason.as_deref() == Some(RESULT_UNKNOWN_REASON)
                    && targets_standard_online_markdownize(
                        store.kio_dir(),
                        task,
                        &online_output_ref(&markdown_profile.adapter_id),
                    )
                    && matches!(
                        classify_online_markdownize_precondition(repo, task),
                        OnlineMarkdownizePrecondition::Send(_)
                    )
            })
            .map(|task| task.task_id.clone())
            .collect::<Vec<_>>(),
        "embedding"
            if embedding_profile
                .as_ref()
                .is_some_and(|profile| profile.profile_hash == key.tool_profile_hash) =>
        {
            current_embedding_unknown_group_task_ids(
                repo,
                &key,
                embedding_profile
                    .as_ref()
                    .expect("profile was checked immediately above"),
                &tasks,
            )?
        }
        _ => {
            return Err(KioError::invalid_usage(
                "--resend-unknown selector does not match an active adapter profile in this scope",
            ));
        }
    };
    if matching_task_ids.is_empty() {
        return Err(KioError::invalid_usage(
            "--resend-unknown requires a current eligible result_unknown task matching the selected ledger row",
        ));
    }
    if key.adapter_kind == "markdownize" && matching_task_ids.len() != 1 {
        return Err(KioError::invalid_usage(
            "--resend-unknown found multiple current markdownize tasks for one ledger row; reconcile duplicate tasks before authorizing a fresh request",
        ));
    }
    confirm_batch_mutation(
        ctx,
        &format!(
            "Authorize a fresh charged sync request after an unknown prior result for {}/{}/{}?",
            key.scope_id, key.adapter_kind, key.input_hash
        ),
        yes,
    )?;
    // Audit precedes the ledger CAS transition. If the process dies before it,
    // no fresh send is authorized; if it dies after it, the idempotent ledger
    // marker lets this exact task-store update safely replay.
    ctx.control.append_event_log_in_session(
        ctx.locks,
        "KIO-I-SYNC-RESULT-UNKNOWN-RESEND-001",
        "operator authorized one fresh sync request after unknown result",
        json!({
            "scope_id": key.scope_id,
            "adapter_kind": key.adapter_kind,
            "input_hash": key.input_hash,
            "tool_profile_hash": key.tool_profile_hash,
            "task_count": matching_task_ids.len(),
        }),
    )?;
    let newly_authorized = authorize_unknown_sync_resend(&ledger, &key).map_err(pipeline_to_kio)?;
    let matching = matching_task_ids.into_iter().collect::<BTreeSet<_>>();
    let queued = store
        .update_matching(|task| {
            if !matching.contains(&task.task_id) {
                return false;
            }
            task.status = TaskStatus::Pending;
            task.fallback_reason = Some(
                if task.task_type == TaskType::Markdownize {
                    SYNC_RESEND_AUTHORIZED_REASON
                } else {
                    "ready_for_online_adapter"
                }
                .to_owned(),
            );
            task.hold_reason = None;
            task.next_retry_at = None;
            task.heartbeat_at = None;
            task.clear_reservation();
            true
        })
        .map_err(pipeline_to_kio)?;
    Ok(json!({
        "status": "resend_authorized",
        "authorization_created": newly_authorized,
        "tasks_queued": queued,
        "scope_id": key.scope_id,
        "adapter_kind": key.adapter_kind,
        "input_hash": key.input_hash,
        "next_step": "run an explicit online resume; it will perform a fresh reservation and current policy, grant, and budget checks before any provider request",
        "warning": "the prior synchronous request may already have been charged",
    }))
}

/// Resolve the sole current embedding content group attached to a result-unknown
/// ledger row. The ledger input hash is the group's canonical embedding hash
/// (text + filename context + active profile), not a representative task's text
/// hash. Recompute that identity from the current retained, policy-allowed chunks;
/// never infer it from a task-input hash prefix or a stale task record.
fn current_embedding_unknown_group_task_ids(
    repo: &Repository,
    key: &LedgerTaskKey,
    profile: &DeclaredEmbeddingProfile,
    tasks: &[TaskDescriptor],
) -> Result<Vec<String>> {
    let db_path = sqlite_path(repo.kio_dir());
    if !db_path.exists() {
        return Err(KioError::invalid_usage(
            "--resend-unknown embedding target has no current source index",
        ));
    }
    let Some(head) = repo.head_commit_hash()? else {
        return Err(KioError::invalid_usage(
            "--resend-unknown embedding target has no current HEAD",
        ));
    };
    let conn = open_existing_source_index_connection(
        &db_path,
        ExistingSourceIndexOpenMode::ReadOnly,
        &FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        },
    )
    .map_err(index_to_kio)?;
    let chunking_config_hash = snapshot_chunking_config_hash(repo, &head)?;
    let retained_instances = retained_history_instances(repo, &head)?;
    let policy = current_embedding_policy(repo)?;
    let retained = retained_history_chunks(
        &conn,
        repo,
        &retained_instances,
        &chunking_config_hash,
        Some(&policy),
    )?;
    let retained = bind_embedding_work(retained, profile)?;
    let pending = retained_chunks_without_embedding(&conn, retained, profile)?;
    let plan = plan_embed_batch(&conn, profile, &pending)
        .map_err(|_| KioError::invalid_usage("cannot establish current embedding content group"))?;

    let mut tasks_by_ref = BTreeMap::<String, Vec<&TaskDescriptor>>::new();
    for task in tasks
        .iter()
        .filter(|task| task.task_type == TaskType::Embedding)
    {
        tasks_by_ref
            .entry(task.output_ref.clone())
            .or_default()
            .push(task);
    }
    let mut candidate_groups = Vec::<Vec<String>>::new();
    for group in plan.to_send {
        if group.embedding_hash != key.input_hash {
            continue;
        }
        let mut group_unknown = Vec::new();
        let mut well_formed = true;
        for member in group.members {
            let output_ref = embedding_task_output_ref(member);
            let Some(member_tasks) = tasks_by_ref.get(&output_ref) else {
                well_formed = false;
                break;
            };
            // Duplicate task rows make the representative and its charge owner
            // non-canonical. Do not choose one implicitly for a paid recovery.
            if member_tasks.len() != 1 {
                well_formed = false;
                break;
            }
            let task = member_tasks[0];
            if task.input_hash != member.text_hash
                || task.changed_unit_keys != [member.chunk_id.clone()]
            {
                return Err(KioError::schema(
                    "embedding recovery task does not match authenticated work",
                ));
            }
            if task.status == TaskStatus::Failed
                && task.fallback_reason.as_deref() == Some(RESULT_UNKNOWN_REASON)
            {
                group_unknown.push(task.task_id.clone());
            }
        }
        if well_formed && !group_unknown.is_empty() {
            candidate_groups.push(group_unknown);
        }
    }
    match candidate_groups.len() {
        1 => Ok(candidate_groups.pop().expect("one group was checked")),
        0 => Err(KioError::invalid_usage(
            "--resend-unknown requires one current eligible embedding content group",
        )),
        _ => Err(KioError::invalid_usage(
            "--resend-unknown embedding ledger key matches multiple current content groups; refuse ambiguous fresh authorization",
        )),
    }
}

/// `kio status`'s stalled-batch section (CL37/CL68): permanently-unresolvable
/// `batch_requests` rows (settled but residue cleanup still pending), each with
/// its `intent_token` so it can be passed straight to `kio batch abandon`.
fn stalled_batch_status_json(
    ledger: Option<&LedgerReadSnapshot>,
    control: &ControlHandle,
) -> Result<Vec<Value>> {
    let Some(ledger) = ledger else {
        return Ok(Vec::new());
    };
    let ledger_path = ledger_db_path(control);
    let rows = ledger
        .stalled_rows()
        .map_err(|error| ledger_snapshot_to_kio(&ledger_path.display().to_string(), error))?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let issued_at_ms = row
                .intent_token
                .as_deref()
                .and_then(uuid_v7_timestamp_millis);
            json!({
                "scope_id": row.key.scope_id,
                "adapter_kind": row.key.adapter_kind,
                "input_hash": row.key.input_hash,
                "tool_profile_hash": row.key.tool_profile_hash,
                "intent_token": row.intent_token,
                "error": row.error,
                "estimated_usd": row.estimated_usd,
                "issued_at_ms": issued_at_ms,
            })
        })
        .collect())
}

/// Q3: reclaim orphaned `Running` tasks back to `Pending`. A task is flipped to
/// `Running` only by `execute_pending_markdownize_tasks`, immediately before an
/// online send, and back to Done/Partial/Failed once it returns. Kio is
/// single-user and every executor (`batch resume` / `batch retry` / index
/// enrichment) holds the folder store lock end-to-end, so any `Running` task
/// observed while holding that lock is necessarily an orphan from a process that
/// died mid-send (`Running` had no outbound transition, an absorbing state).
/// Reclaim it to `Pending` — clearing the stale `heartbeat_at` — so it is retried
/// instead of being stuck forever and dragging `enriched_ratio` below 1.0.
fn reclaim_orphaned_running_tasks(store: &TaskStore) -> Result<usize> {
    store
        .update_matching(|task| {
            if task.status == TaskStatus::Running {
                task.status = TaskStatus::Pending;
                task.heartbeat_at = None;
                true
            } else {
                false
            }
        })
        .map_err(pipeline_to_kio)
}

/// R9-4 / R10-4: convert incomplete, known online Markdownize results back into
/// retryable Pending tasks. Both Partial and all-Failed responses are normalized
/// immutable instances whose manifest records each failed unit's real `error_kind`.
/// Recovery uses that unit plan only: generic Failed-task retry must never flatten a
/// known normalized response into a synthetic task-level failure. A non-retryable unit
/// (invalid_input / ...) remains terminal; finite budgets stop repeated paid requests,
/// while the explicitly declared per-unit `rate_limit` policy remains unlimited.
fn reenqueue_incomplete_markdownize_tasks(store: &TaskStore) -> Result<usize> {
    let online_ref = online_output_ref(&online_markdownize_profile().adapter_id);
    // Collect each Partial task's retry plan first — this needs manifest I/O, so it
    // is done outside the `update_matching` closure.
    let mut plan_by_id: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for task in store.all().map_err(pipeline_to_kio)? {
        if !matches!(task.status, TaskStatus::Partial | TaskStatus::Failed)
            || task.task_type != TaskType::Markdownize
            || task.fallback_reason.as_deref() != Some("online_adapter_done")
            || !targets_standard_online_markdownize(store.kio_dir(), &task, &online_ref)
        {
            continue;
        }
        let plan = partial_retry_plan_from_instance(store, &task)?;
        // Gate 1 (R10-4): no retryable Failed unit -> leave the task incomplete.
        if plan.retryable_units.is_empty() {
            continue;
        }
        // Gate 2 (R10-4): the retry budget for the governing kind is spent -> leave
        // the task incomplete (static). `None` means at least one retryable unit has an
        // unlimited budget, so the cap never trips.
        if plan.max_attempts.is_some_and(|max| task.attempts >= max) {
            continue;
        }
        plan_by_id.insert(task.task_id.clone(), plan.retryable_units);
    }
    if plan_by_id.is_empty() {
        return Ok(0);
    }
    store
        .update_matching(|task| {
            let Some(retryable) = plan_by_id.get(&task.task_id) else {
                return false;
            };
            task.status = TaskStatus::Pending;
            task.unit_keys = Some(retryable.clone());
            // Preserve the typed normalized-instance reference. The retry precondition
            // needs its complete manifest to recover every provider-discovered unit;
            // replacing this with the online placeholder made OCR-from-scratch partial
            // tasks impossible to resume.
            task.fallback_reason = None;
            task.next_retry_at = None;
            // R10-4/R19-5: the re-drive is charged against the retry budget in the
            // executor's result handling (a single accounting point) — on `Err` (full
            // send failure) OR `Ok(Partial)` (units still missing). Pre-incrementing here
            // double-counted with the `Err` path, exhausting `max_attempts` twice as fast.
            true
        })
        .map_err(pipeline_to_kio)
}

/// The retryable subset of a Partial normalized instance's Failed units plus the
/// governing retry budget (R10-4). Used by `reenqueue_incomplete_markdownize_tasks`.
struct PartialRetryPlan {
    /// Failed unit_keys whose recorded `error_kind` is retryable (docs/04 §5.2).
    retryable_units: Vec<String>,
    /// The smallest finite `max_attempts` among those units' kinds, or `None` when at
    /// least one kind has an unlimited retry budget.
    max_attempts: Option<u32>,
}

/// Read a normalized instance's manifest (`output_ref/manifest.json`) and build its
/// [`PartialRetryPlan`]: the still-Failed units filtered to the retryable ones (by
/// their recorded `error_kind`) and the retry-budget cap. A missing manifest or a
/// fully-Done instance yields an empty plan.
fn partial_retry_plan_from_instance(
    task_store: &TaskStore,
    task: &TaskDescriptor,
) -> Result<PartialRetryPlan> {
    let empty = PartialRetryPlan {
        retryable_units: Vec::new(),
        max_attempts: Some(0),
    };
    let identity = validate_task_output_ref(task_store.kio_dir(), task).map_err(pipeline_to_kio)?;
    let TaskOutputRef::NormalizedInstance {
        raw_hash,
        tool_profile_hash,
        r#gen,
        ..
    } = identity
    else {
        return Ok(empty);
    };
    let manifest = load_validated_normalized_instance_with_control(
        task_store.kio_dir(),
        &raw_hash,
        &tool_profile_hash,
        r#gen,
        task_store.control(),
    )
    .map_err(pipeline_to_kio)?
    .manifest;
    Ok(partial_retry_plan_from_manifest(&manifest, task.attempts))
}

/// Select only units that still have their own retry budget remaining. The task
/// attempt counter is shared across a unit-scoped task, so it is conservative:
/// an exhausted finite unit is excluded even when a sibling has an unlimited
/// policy. This prevents a `rate_limit` sibling from extending NetworkError's
/// declared five-attempt budget without adding new per-unit mutable state.
fn partial_retry_plan_from_manifest(
    manifest: &NormalizedInstanceManifest,
    task_attempts: u32,
) -> PartialRetryPlan {
    let mut retryable_units = Vec::new();
    let mut finite_max: Option<u32> = None;
    let mut saw_unlimited = false;
    for entry in &manifest.units {
        if entry.status == UnitStatus::Done {
            continue;
        }
        let policy = retry_policy(retry_kind_from_reason(entry.error_kind.as_deref()));
        if !policy.retryable || policy.max_attempts.is_some_and(|max| task_attempts >= max) {
            continue;
        }
        retryable_units.push(entry.unit_key.clone());
        match policy.max_attempts {
            None => saw_unlimited = true,
            Some(max) => finite_max = Some(finite_max.map_or(max, |current| current.min(max))),
        }
    }
    let max_attempts = if retryable_units.is_empty() {
        Some(0)
    } else if saw_unlimited {
        None
    } else {
        finite_max
    };
    PartialRetryPlan {
        retryable_units,
        max_attempts,
    }
}

/// Classify an accepted, already-persisted online result for batch reporting.
/// Its immutable manifest is the source of retryability; settlement has already
/// happened before this function and is deliberately not repeated.
fn known_markdownize_completion(store: &TaskStore, task_id: &str) -> Result<ExecOutcome> {
    let task = store
        .all()
        .map_err(pipeline_to_kio)?
        .into_iter()
        .find(|task| task.task_id == task_id)
        .ok_or_else(|| KioError::schema("completed markdownize task disappeared"))?;
    if task.status == TaskStatus::Done {
        return Ok(ExecOutcome {
            executed: 1,
            ..ExecOutcome::default()
        });
    }
    let mut outcome = ExecOutcome {
        failed: 1,
        ..ExecOutcome::default()
    };
    if matches!(task.status, TaskStatus::Partial | TaskStatus::Failed)
        && !partial_retry_plan_from_instance(store, &task)?
            .retryable_units
            .is_empty()
    {
        outcome.failed_retryable = 1;
    }
    Ok(outcome)
}

/// R9-7: outcome counts from an enrichment pass. `batch retry`/`resume` need more
/// than the success count to be honest: a driven task can fail (an online send was
/// attempted, rate-limit / auth consumed) yet the old JSON reported only
/// `tasks_executed`, so an orchestrator could not see the attempt. `executed`
/// keeps its meaning (tasks completed OK == `tasks_executed`); `failed` is tasks
/// that transitioned to Failed this pass; `attempted()` = executed + failed
/// (budget-paused / held tasks that issued no adapter call are excluded).
#[derive(Debug, Default, Clone, Copy)]
struct ExecOutcome {
    executed: usize,
    failed: usize,
    /// R11-2: tasks whose online send this pass was halted by the budget cap
    /// (transitioned to Paused/budget_exceeded). Excluded from `attempted()` — no
    /// adapter call was issued. Drives docs/04 §5.6 exit 6.
    paused: usize,
    /// R11-2: subset of `failed` whose error was auth/authorization (docs/04 §5.6
    /// exit 5 — needs user re-auth, distinct from a retryable transient).
    auth_failed: usize,
    /// R11-2: subset of `failed` that is retry-eligible (retryable kind + attempts
    /// left). `failed - failed_retryable` is the permanently-failed remainder; the
    /// split drives the batch exit-code choice 3 (some retryable) vs 4 (all permanent).
    failed_retryable: usize,
    /// Batch-lane tasks left in flight by this pass (04 §5.8): freshly
    /// submitted jobs and polled still-QUEUED/RUNNING jobs. Excluded from
    /// `attempted()` (nothing terminal happened), but counted as
    /// retryable-remaining for the batch exit choice — "in-flight のみ残"
    /// exits 3 exactly like a retryable failure would, since a later
    /// `kio batch resume` is what makes progress.
    inflight: usize,
    /// Subset of `inflight` held because the provider's response could NOT be
    /// read — not because the job is still running. 04 §5.8's unknown rule says
    /// to hold and retry such a row, which is right, but a bare hold is
    /// indistinguishable from a healthy queued job and so hides a permanent
    /// fault behind an ordinary-looking count.
    ///
    /// Two real defects reached production behind exactly that silence (a
    /// result parser that could not read the live shape, then a size bound that
    /// rejected any job past ~20 members): in both cases the provider had every
    /// vector ready and `tasks_inflight` simply never dropped. A non-zero value
    /// here means "stop waiting and look", not "wait longer".
    held_unreadable: usize,
}

impl ExecOutcome {
    fn attempted(self) -> usize {
        self.executed + self.failed
    }

    fn add(&mut self, other: ExecOutcome) {
        self.executed += other.executed;
        self.failed += other.failed;
        self.paused += other.paused;
        self.auth_failed += other.auth_failed;
        self.failed_retryable += other.failed_retryable;
        self.inflight += other.inflight;
        self.held_unreadable += other.held_unreadable;
    }
}

/// R11-2: the batch exit code (docs/04 §5.6 / docs/06 §7) a `batch resume`/`retry`
/// pass reports, computed from the tasks it TOUCHED this pass (its `ExecOutcome`,
/// not the whole store — a pass that leaves a pre-existing paused/failed task
/// untouched changed nothing and exits 0, preserving the L2 sticky-pause symmetry).
/// Priority, highest first: auth (5, user re-auth), then budget-paused (6, needs
/// `--override-budget`), then some-retryable-failed (3, `batch retry` can recover),
/// then all-permanent-failed (4). A clean pass returns `None` = exit 0.
fn batch_exit_override(outcome: &ExecOutcome) -> Option<ExitCode> {
    if outcome.auth_failed > 0 {
        Some(ExitCode::AuthError)
    } else if outcome.paused > 0 {
        Some(ExitCode::BudgetExceeded)
    } else if outcome.failed_retryable > 0 || outcome.inflight > 0 {
        // Batch-lane in-flight jobs (submitted/queued/running — 04 §5.8) are
        // retryable-remaining work in exactly the docs/04 §5.6 exit-3 sense:
        // "retryable な失敗が残っている" — a later `kio batch resume` poll is
        // what completes them.
        Some(ExitCode::PartialFailure)
    } else if outcome.failed > 0 {
        Some(ExitCode::PermanentFailure)
    } else {
        None
    }
}

/// R14-5: the batch-owned observability `error_code` for a Partial(3)/Permanent(4) batch
/// exit, mirroring how `index` self-sets `KIO-E-INDEX-PARTIAL-001` on its partial exit.
/// Without it, `append_exit_override_error` fell back to `exit_override_error_code`, which
/// labels 3/4 with SEARCH codes (`KIO-E-SEARCH-PARTIAL-001` — not even in the catalog —
/// and `KIO-E-SEARCH-SCOPE-ALL-FAILED-001`, the multi-scope-search all-failed code), so a
/// batch task failure was mis-classified as a search failure in errors.jsonl. Auth(5) /
/// budget(6) keep their shared codes (`KIO-E-ADAPTER-AUTH-001` / `KIO-E-BUDGET-EXCEEDED-001`),
/// which are correct for batch too, so they return `None` and use the shared fallback.
fn batch_error_code(code: ExitCode) -> Option<&'static str> {
    match code {
        ExitCode::PartialFailure => Some("KIO-E-BATCH-PARTIAL-001"),
        ExitCode::PermanentFailure => Some("KIO-E-BATCH-TASK-FAILED-001"),
        _ => None,
    }
}

/// R14-5: apply the batch exit override to `output`, carrying a batch-owned `error_code`
/// (so errors.jsonl classifies the failure as batch, not search) before embedding the
/// `__exit_code` marker. Shared by `batch resume` and `batch retry`.
fn apply_batch_exit_override(output: &mut Value, outcome: &ExecOutcome) {
    if let Some(code) = batch_exit_override(outcome) {
        if let Some(error_code) = batch_error_code(code) {
            output["error_code"] = json!(error_code);
        }
        set_exit_override(output, code);
    }
}

/// R11-2: the exit code `index`/`repair`/`reindex` reports for its inline enrichment
/// (embedding) outcome. Unlike `batch`, a retryable/permanent embedding failure does
/// NOT override the exit — docs/05 says enrichment failure never aborts the local
/// index; it is disclosed in the result JSON and left for `batch resume` (exit 0).
/// Only the two states that need user intervention override the exit while the full
/// result JSON still prints to stdout (the search-exit-3 "result + nonzero" pattern):
/// auth (5) and budget-pause (6).
fn enrichment_exit_override(outcome: &ExecOutcome) -> Option<ExitCode> {
    if outcome.auth_failed > 0 {
        Some(ExitCode::AuthError)
    } else if outcome.paused > 0 {
        Some(ExitCode::BudgetExceeded)
    } else {
        None
    }
}

/// Embed the private `__exit_code` marker (stripped by `take_exit_override` before
/// printing) so a command prints its full result JSON to stdout yet exits non-zero
/// (05 §1.8 search pattern, extended by R11-2/R11-3 to index/repair/reindex/batch).
fn set_exit_override(output: &mut Value, code: ExitCode) {
    if let Some(object) = output.as_object_mut() {
        object.insert("__exit_code".to_owned(), json!(code.code()));
    }
}

fn execute_pending_tasks(
    invocation: Invocation<'_>,
    repo: &Repository,
    store: &TaskStore,
    override_budget: bool,
    // R22-6(a): `batch resume` means "carry on with the work", so it may revive a task the
    // operator has since unblocked by fixing credentials. `batch retry` may NOT — CT2-TASK-005
    // (docs/04 §5.6, `auth_error: max_attempts=0`) makes non-retryability of an auth failure a
    // contract of that command specifically.
    allow_auth_revive: bool,
    // QA30 (step4b-contract-tests-p3a.md §I, 06-cli-spec.md §1 L21-30,
    // 07-adapter-spec.md §3 L220-222): `kio batch resume`/`kio batch retry`'s
    // own `--online`/`--offline` one-shot embedding opt-in for this pass.
    // Mutually exclusive, validated by the caller before this is reached.
    online: bool,
    offline: bool,
) -> Result<ExecOutcome> {
    let ctx = &invocation;
    let adapter_settings = invocation.settings;

    let mut outcome = ExecOutcome::default();
    // Q3: under the folder store lock, any Running task is an orphan from a crashed
    // run — reclaim it to Pending so this pass re-executes it.
    reclaim_orphaned_running_tasks(store)?;
    if recover_pending_online_promotion(repo)? {
        finish_pending_online_promotion(ctx, repo)?;
    }
    // Markdownize and embedding opt-ins are per-adapter (07 §3, L4): gate each
    // adapter on its own approval rather than one blanket check.
    if !offline && persistent_network_allowed(adapter_settings, repo)? {
        outcome.add(execute_pending_markdownize_tasks(
            ctx,
            repo,
            store,
            override_budget,
            allow_auth_revive,
        )?);
    }
    // Online Markdownize changes durable search truth only after the complete
    // output is rebound into one auto commit and SQLite is rebuilt. Do this before
    // embedding enrichment so the same batch pass can see/promote the new chunks.
    if promote_completed_online_markdownize(ctx, repo)? {
        finish_pending_online_promotion(ctx, repo)?;
    }
    // Embedding tasks are executed by the same enrichment pass `kio index` uses;
    // without this, rate-limited Pending embedding tasks could never be completed
    // by `batch resume` / `batch retry`. `override_budget` reaches the budget
    // judgement (L2) and the embedding opt-in is the embedding adapter's own (L4).
    // QA30: `online`/`offline` (this pass's flags) replace the previous
    // hard-coded `(false, false, false)` — `batch resume`/`retry` carry no
    // same-invocation `--yes`/`--approve`-equivalent flag, so
    // `online_confirmed` stays `false` (matching `kio index`'s own
    // `--online`-without-`--approve` behavior: `--online` alone still needs
    // an existing approval row).
    let embedding_online =
        embedding_online_allowed(adapter_settings, repo, offline, online, false)?;
    outcome.add(run_embedding_enrichment(
        ctx,
        repo,
        embedding_online,
        override_budget,
        allow_auth_revive,
    )?);
    Ok(outcome)
}

/// Execute Pending online markdownize tasks, honoring `override_budget` (L2 i).
/// R9-7: returns success + failure counts so `batch` JSON can report driven
/// attempts, not only successes.
fn execute_pending_markdownize_tasks(
    ctx: &Invocation<'_>,
    repo: &Repository,
    store: &TaskStore,
    override_budget: bool,
    allow_auth_revive: bool,
) -> Result<ExecOutcome> {
    let adapter_settings = ctx.settings;

    let budget_caps = read_budget_policy(
        user_config_toml_path(ctx.control),
        repo.kio_dir().join("config.toml"),
    )
    .map_err(pipeline_to_kio)?;
    let ledger = open_ledger_db(ctx.control)?;
    let month = utc_month(&ctx.control.now_utc_seconds());
    let scope_id = repo.scope_identity()?.scope_id;
    // N1a (defense in depth): even a Pending online markdownize task must not be
    // sent when its input is a Tier B (candidate-secret) file and the scope is not
    // `--send-secrets`-approved — in case the hold was cleared by some other path.
    let online_profile = online_markdownize_profile_for(repo)?;
    let secrets_approved = secrets_send_approved(
        adapter_settings,
        repo,
        approvals::AdapterRole::Markdown,
        &online_profile.adapter_id,
        &online_profile.tool_profile_hash,
    );
    let output_ref = online_output_ref(&online_profile.adapter_id);
    // R22-6(a), updated for QA2 (step4b-contract-tests-p3a.md §A, 04 §5.2):
    // R21-6 gave the embedding pipeline a live-AuthError revive but never wired
    // the markdownize twin, so a 401/403 left its task stranded — with its
    // reservation eating the month's cap. Under QA2 an auth failure lands
    // `Paused(hold_reason=auth)` (never `Failed`), and `run_batch`'s Resume arm
    // already generically flips ANY non-budget non-secret Paused task back to
    // Pending BEFORE this function runs — so that flip is normally what revives
    // it. This block is the belt-and-braces for a caller that reaches
    // `execute_pending_markdownize_tasks` WITHOUT going through that flip (and
    // for any Paused(auth) row the flip's own iteration order left unrevived
    // this same pass), and it still uniquely owns the ledger release: 401/403
    // is refused before billing (R20-3), so releasing its still-open ledger row
    // (if any) settles it `unknown_settled` rather than leaving it stranded
    // open. Only a task whose precondition is not `Retire` (file present,
    // unedited, within the cap) is revived; a genuinely stale one is retired by
    // the loop below as before.
    let markdownize_adapter_kind = "markdownize";
    let markdown_profile_hash = online_profile.tool_profile_hash.clone();
    let auth_revivable = if allow_auth_revive {
        store
            .all()
            .map_err(pipeline_to_kio)?
            .into_iter()
            .filter(|task| {
                task.task_type == TaskType::Markdownize
                    && targets_standard_online_markdownize(store.kio_dir(), task, &output_ref)
                    && task.status == TaskStatus::Paused
                    && retry_kind_from_reason(task.fallback_reason.as_deref())
                        == RetryErrorKind::AuthError
                    && !matches!(
                        classify_online_markdownize_precondition(repo, task),
                        OnlineMarkdownizePrecondition::Retire
                    )
            })
            .map(|task| task.task_id)
            .collect::<BTreeSet<_>>()
    } else {
        BTreeSet::new()
    };
    if !auth_revivable.is_empty() {
        for task in store.all().map_err(pipeline_to_kio)? {
            if auth_revivable.contains(&task.task_id) {
                let key = task_ledger_key(
                    &scope_id,
                    markdownize_adapter_kind,
                    &task.input_hash,
                    &markdown_profile_hash,
                );
                release_task_charge_if_open(&ledger, &key)?;
            }
        }
        store
            .update_matching(|task| {
                if !auth_revivable.contains(&task.task_id) {
                    return false;
                }
                task.clear_reservation();
                task.status = TaskStatus::Pending;
                task.fallback_reason = Some("ready_for_online_adapter".to_owned());
                task.hold_reason = None;
                task.attempts = 0;
                task.next_retry_at = None;
                task.heartbeat_at = None;
                true
            })
            .map_err(pipeline_to_kio)?;
    }
    let tasks = store
        .all()
        .map_err(pipeline_to_kio)?
        .into_iter()
        .filter(|task| {
            task.status == TaskStatus::Pending
                && task.task_type == TaskType::Markdownize
                && targets_standard_online_markdownize(store.kio_dir(), task, &output_ref)
                // Honor an unelapsed retry backoff even for a Pending task
                // (Step2c I2); `batch retry` already gates on this, so this is
                // a defensive belt-and-braces guard.
                && task_retry_due(task, ctx.control)
                // R19-1: the send-time defensive re-check must exclude ANY unapproved
                // secret (Tier B or a lifted Tier A), not just Tier B.
                && (secrets_approved
                    || classify_secret(&task.input_path).is_none())
        })
        .collect::<Vec<_>>();
    let mut counts = ExecOutcome::default();
    // Batch send lane (04 §5.8 / 07 §5.5): resolved once per pass — `Some`
    // exactly when this pass can drive upload → job-create against a
    // configured (mock or real) Batch client.
    let batch_client = batch_markdownize_lane_client(adapter_settings, &ledger)?;
    for task in tasks {
        let task_id = task.task_id.clone();
        let key = task_ledger_key(
            &scope_id,
            markdownize_adapter_kind,
            &task.input_hash,
            &markdown_profile_hash,
        );
        // R15-2: verify the network-free preconditions BEFORE reserving the cost. The
        // reservation lands under a `BEGIN IMMEDIATE` Tx BEFORE the send, but a stale
        // (edited-after-enqueue), text-native, or unpreparable task is superseded by
        // R14-2 inside the executor WITHOUT ever calling the adapter — so charging
        // first is a phantom charge that double-bills and can exhaust the markdownize
        // cap, falsely pausing the valid task. Fail the task here (non-retryable
        // invalid_input; the recovery is a re-index, not a retry) instead of charging
        // + entering the executor. This mirrors the executor's own R14-2/R9-2 guards,
        // hoisted ahead of the charge.
        let prepared_input = match classify_online_markdownize_precondition(repo, &task) {
            OnlineMarkdownizePrecondition::Send(prepared_input) => prepared_input,
            OnlineMarkdownizePrecondition::AwaitConversion => {
                // A live Office document remains honest Pending work until its bounded
                // local conversion contract exists. Nothing was sent or charged.
                continue;
            }
            OnlineMarkdownizePrecondition::Retire => {
                // R18-2: this task may carry a still-open reservation from a PRIOR
                // failed send (its file was deleted/edited after that attempt).
                // Release it here — settling `unknown_settled` at the reservation
                // estimate if a row is still open, a no-op otherwise — BEFORE the
                // retirement below; no NEW charge is reserved this pass (the
                // precondition failed before the charge).
                release_task_charge_if_open(&ledger, &key)?;
                store
                    .update_matching(|candidate| {
                        if candidate.task_id == task_id {
                            retire_online_task(candidate);
                            candidate.attempts = candidate.attempts.saturating_add(1);
                            true
                        } else {
                            false
                        }
                    })
                    .map_err(pipeline_to_kio)?;
                counts.failed += 1;
                continue;
            }
            OnlineMarkdownizePrecondition::ConversionFailed => {
                // 07 §5.1: mirrors the generic `TaskExecutionFailure` retry-kind
                // handling below (the `_ => {...}` arm after the adapter send),
                // applied here because the failure was discovered BEFORE any
                // adapter call — the same contract_violation retry accounting
                // (04 §5.3: retryable, one retry), but no ledger charge was ever
                // reserved this pass (mirrors `Retire` immediately above: only a
                // still-open reservation from a PRIOR attempt is released).
                release_task_charge_if_open(&ledger, &key)?;
                let policy = retry_policy(RetryErrorKind::ContractViolation);
                let attempts_after = task.attempts.saturating_add(1);
                let next_retry_at = (policy.retryable
                    && policy
                        .max_attempts
                        .map(|max| attempts_after < max)
                        .unwrap_or(true))
                .then(|| {
                    scheduled_retry_at(
                        &ctx.control.now_utc_seconds(),
                        &policy.backoff,
                        attempts_after,
                    )
                });
                let reason = retry_reason(RetryErrorKind::ContractViolation).to_owned();
                store
                    .update_matching(|candidate| {
                        if candidate.task_id == task_id {
                            candidate.status = TaskStatus::Failed;
                            candidate.fallback_reason = Some(reason.clone());
                            candidate.hold_reason = None;
                            candidate.heartbeat_at = None;
                            candidate.attempts = attempts_after;
                            candidate.next_retry_at = next_retry_at.clone();
                            candidate.clear_reservation();
                            true
                        } else {
                            false
                        }
                    })
                    .map_err(pipeline_to_kio)?;
                counts.failed += 1;
                if next_retry_at.is_some() {
                    counts.failed_retryable += 1;
                }
                continue;
            }
        };
        let file_size = prepared_input.bytes.len() as u64;
        // R11-6: prorate the reserved cost of a UNIT-SCOPED retry by the fraction of
        // the document actually re-sent — `unit_keys` names the still-failed units,
        // and `execute_online_markdownize_task` requests only those. A full send
        // (`unit_keys == None`) still bills the whole document. Without this, a
        // 1-page retry of a 500-page PDF re-billed all 500 pages. Only used as the
        // *candidate* for a FRESH reservation below — CL42/CL44: an already-open row
        // from a prior attempt (`reserve_or_reuse_task_charge`'s reuse path) is
        // billed at ITS OWN stored `estimated_usd`, not recomputed here.
        let candidate_usd = prorated_markdownize_cost(
            &task,
            file_size,
            prepared_input.prepared_units.len(),
            task.bbox_annotation_enabled.unwrap_or(false),
        );
        let caps = budget_cap_config(&budget_caps, &scope_id, markdownize_adapter_kind);
        // F5: `hard_stop = false` (soft-stop) bypasses a cap denial exactly like
        // `--override-budget` does — see `reserve_or_reuse_task_charge`'s doc.
        let bypass_cap_denial = override_budget || !budget_caps.hard_stop;
        // Lane selection (04 §5.8 / 07 §5.5): decided BEFORE the reservation so
        // phase 1 records the matching `request_kind` ('batch' rows are what the
        // §5.8 recovery/poll machinery keys on).
        let existing_row = get_batch_request(&ledger, &key).map_err(pipeline_to_kio)?;
        if sync_result_unknown_fenced(existing_row.as_ref()) {
            mark_sync_result_unknown_task(store, &task_id)?;
            counts.failed += 1;
            continue;
        }
        if existing_row
            .as_ref()
            .is_some_and(|row| row.state.is_terminal() && row.intent_token.is_some())
        {
            // §5.8 順序規範 (cleanup-first): a terminal row whose residue
            // cleanup is still outstanding blocks re-submission — a fresh
            // phase 1 would be refused (KIO-E-BATCH-CLEANUP-PENDING-001), so
            // skip the task this pass instead of failing the whole command.
            // The stalled row is visible in `kio status`; the poll pass's
            // cleanup replay, `kio ledger reconcile`, or `kio batch abandon`
            // complete the cleanup and unblock the resend.
            counts.inflight += 1;
            continue;
        }
        let lane = markdownize_send_lane(
            batch_client.is_some(),
            existing_row.as_ref(),
            realtime_lane_requested(),
        );
        if lane == MarkdownizeSendLane::DeferToRecovery {
            counts.inflight += 1;
            continue;
        }
        // A fresh sync send has no provider-side residue yet. Resolve the
        // declared credential locally before creating its phase-1 reservation:
        // an unset/empty credential cannot reach the HTTP client, so recording
        // a charge here would be a phantom. Do not apply this admission to an
        // existing sync row: that row may cover an earlier request whose result
        // remains uncertain, and must retain the normal unknown settlement.
        if lane == MarkdownizeSendLane::Sync
            && !sync_markdownize_credential_available(adapter_settings, ctx.control)?
        {
            let reason = retry_reason(RetryErrorKind::AuthError).to_owned();
            store
                .update_matching(|candidate| {
                    if candidate.task_id == task_id {
                        candidate.status = TaskStatus::Paused;
                        candidate.hold_reason = Some(HoldReason::Auth);
                        candidate.fallback_reason = Some(reason.clone());
                        candidate.heartbeat_at = None;
                        candidate.next_retry_at = None;
                        candidate.clear_reservation();
                        true
                    } else {
                        false
                    }
                })
                .map_err(pipeline_to_kio)?;
            counts.failed += 1;
            counts.auth_failed += 1;
            continue;
        }
        let request_kind = if lane == MarkdownizeSendLane::Batch {
            RequestKind::Batch
        } else {
            RequestKind::Sync
        };
        let charge = reserve_or_reuse_task_charge(
            ctx,
            &ledger,
            &key,
            candidate_usd,
            &caps,
            bypass_cap_denial,
            request_kind,
        )?;
        let (intent_token, reserved_usd) = match charge {
            TaskChargeOutcome::BudgetExceeded => {
                store
                    .update_matching(|candidate| {
                        if candidate.task_id == task_id {
                            candidate.status = TaskStatus::Paused;
                            candidate.fallback_reason = Some("budget_exceeded".to_owned());
                            candidate.hold_reason = Some(HoldReason::Budget);
                            true
                        } else {
                            false
                        }
                    })
                    .map_err(pipeline_to_kio)?;
                // R11-2: a Pending task budget-paused THIS pass → docs/04 §5.6 exit 6.
                counts.paused += 1;
                continue;
            }
            TaskChargeOutcome::Reserved {
                intent_token,
                estimated_usd,
            }
            | TaskChargeOutcome::Reused {
                intent_token,
                estimated_usd,
            } => (intent_token, estimated_usd),
        };
        if lane == MarkdownizeSendLane::Batch {
            let client = batch_client
                .as_deref()
                .expect("Batch lane selection guarantees a configured client");
            match submit_batch_markdownize_job(
                adapter_settings,
                client,
                &ledger,
                &key,
                &intent_token,
                &task,
                &prepared_input,
            )? {
                Ok(()) => {
                    // Submitted (or already in flight): the task stays Pending
                    // with the poll cadence as its schedule — a later pass's
                    // `poll_batch_markdownize_jobs` collects it. The
                    // reservation stamps mirror the sync Running flip so purge
                    // and diagnostics can find the live ledger row.
                    let poll_at = batch_poll_retry_at(ctx.control);
                    store
                        .update_matching(|candidate| {
                            if candidate.task_id == task_id {
                                candidate.status = TaskStatus::Pending;
                                candidate.fallback_reason = Some(BATCH_SUBMITTED_REASON.to_owned());
                                candidate.hold_reason = None;
                                candidate.heartbeat_at = None;
                                candidate.next_retry_at = Some(poll_at.clone());
                                candidate.reservation_id = Some(intent_token.clone());
                                candidate.reserved_usd = Some(reserved_usd);
                                candidate.reserved_month = Some(month.clone());
                                true
                            } else {
                                false
                            }
                        })
                        .map_err(pipeline_to_kio)?;
                    counts.inflight += 1;
                }
                Err(error) => {
                    // 相 2a/2b records stay in place (= the §5.8 crash window a
                    // later recovery reconciles), so the ledger row is NEVER
                    // settled here — `settle_on_terminal: false`.
                    record_online_send_failure(
                        store,
                        &ledger,
                        &key,
                        &intent_token,
                        reserved_usd,
                        &task_id,
                        task.attempts,
                        &error,
                        &mut counts,
                        false,
                    )?;
                }
            }
            continue;
        }
        store
            .update_matching(|candidate| {
                if candidate.task_id == task_id {
                    candidate.status = TaskStatus::Running;
                    candidate.heartbeat_at = Some(ctx.control.now_utc_seconds());
                    candidate.fallback_reason = None;
                    // Stamp the live reservation's ledger selector (purge and any
                    // future diagnostics look it up by `intent_token`, not by
                    // reconstructing the ledger key from possibly-since-changed
                    // config — see `purge.rs`'s `delete_target_tasks`). `reserved_usd`/
                    // `reserved_month` accompany it only because
                    // `task::validate_task_descriptor` requires all three stamps
                    // present together or all absent; the ledger's own
                    // `batch_requests.estimated_usd` is the actual source of truth.
                    candidate.reservation_id = Some(intent_token.clone());
                    candidate.reserved_usd = Some(reserved_usd);
                    candidate.reserved_month = Some(month.clone());
                    true
                } else {
                    false
                }
            })
            .map_err(pipeline_to_kio)?;
        // QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880): thread this
        // send's ledger `intent_token` (stable across a crash-window resend —
        // see `MarkdownizeRequest::idempotency_token`'s doc) into the adapter
        // boundary, so a provider that declares a sync idempotency key can be
        // required to carry it.
        match execute_online_markdownize_task(
            adapter_settings,
            repo,
            &task,
            prepared_input,
            Some(intent_token.clone()),
        ) {
            Ok(outcome) => {
                // QA17/QA18/QA19 (step4b-contract-tests-p3a.md §F): bill the
                // adapter's self-reported `usage` when it provided one
                // (Mistral OCR's real processed-page count), converted to
                // USD via tools.toml's declared `[pricing]` — degrading to
                // the reservation estimate exactly as before when `usage` is
                // absent/malformed/uncovered (`resolve_billing_from_reported_usage`'s
                // existing CL27/CL28 rules).
                let markdownize_profile = online_markdownize_profile_for(repo)?;
                // This arm IS the synchronous send, so it settles at the
                // standard rate. Reading the declaration raw made that true by
                // accident and made the batch arm below wrong.
                let markdownize_pricing =
                    lane_adjusted_pricing(adapter_settings, "markdown", PreferredRequestKind::Sync);
                let (billed, uncovered_pricing_kinds) = resolve_billing_from_reported_usage(
                    outcome.usage.as_ref(),
                    &declared_billable_kind_strings(&markdownize_profile),
                    &markdownize_pricing,
                    reserved_usd,
                );
                warn_uncovered_pricing_kinds_once(
                    "markdownize",
                    &uncovered_pricing_kinds,
                    ctx.control,
                    ctx.locks,
                );
                settle_task_charge_success(&ledger, &key, &intent_token, billed)?;
                store
                    .update_matching(|candidate| {
                        if candidate.task_id == task_id {
                            candidate.status = outcome.status;
                            candidate.output_ref = outcome.output_ref.clone();
                            candidate.fallback_reason = Some("online_adapter_done".to_owned());
                            candidate.heartbeat_at = None;
                            candidate.clear_reservation();
                            // R19-5: a unit-scoped retry that still returns Partial (units
                            // missing) consumed one re-drive of the retry budget — advance
                            // `attempts` HERE, the single accounting point, so it converges
                            // on `max_attempts` (the Err arm below does the same). Pre-R19-5
                            // this lived in the incomplete-manifest reenqueue path and
                            // double-counted with the Err arm.
                            if matches!(outcome.status, TaskStatus::Partial | TaskStatus::Failed) {
                                candidate.attempts = candidate.attempts.saturating_add(1);
                            }
                            // R13-1: record the normalization-run provenance
                            // (docs/04:474-479) so `status` and the next incremental
                            // decision (consecutive count) can see mode / previous /
                            // changed units. `Full` clears any prior incremental mark.
                            candidate.mode = Some(outcome.mode);
                            candidate.previous_raw_hash = outcome.previous_raw_hash.clone();
                            candidate.parent_run_id = outcome.parent_run_id.clone();
                            candidate.changed_unit_keys = outcome.changed_unit_keys.clone();
                            true
                        } else {
                            false
                        }
                    })
                    .map_err(pipeline_to_kio)?;
                counts.add(known_markdownize_completion(store, &task_id)?);
            }
            Err(error) => {
                record_sync_markdownize_failure(
                    store,
                    &ledger,
                    &key,
                    &intent_token,
                    reserved_usd,
                    &task_id,
                    task.attempts,
                    &error,
                    &mut counts,
                )?;
            }
        }
    }
    Ok(counts)
}

/// Settle a provider-confirmed sync rejection as zero-cost before any retry.
/// Only typed adapter outcomes may take this path; transport text is never
/// interpreted as a rejection.
fn settle_sync_definitive_rejection(
    ledger: &LedgerDb,
    key: &LedgerTaskKey,
    intent_token: &str,
    kind: RetryErrorKind,
) -> Result<()> {
    let error = match kind {
        RetryErrorKind::AuthError => "sync_auth_rejected",
        RetryErrorKind::RateLimit => "sync_rate_limited",
        RetryErrorKind::QuotaExceeded => "sync_quota_rejected",
        _ => return Err(KioError::schema("sync rejection kind is not definitive")),
    };
    terminal_transaction(
        ledger,
        &TerminalWrite {
            key,
            outcome: Outcome::SubmitRejected,
            billed: BilledAmount {
                usd: 0.0,
                estimated: false,
            },
            ledger_batch_job_id: intent_token,
            next_state: BatchState::Terminal,
            error: Some(error),
            increment_contract_violation: false,
            attempts_delta: 1,
            clear_intent_token: true,
            intent_token_guard: Some(intent_token),
            reseat_submission_seq: false,
        },
    )
    .map_err(pipeline_to_kio)
    .map(|_| ())
}

/// Record a post-send sync failure. Non-idempotent transport, persistence, and
/// response-contract failures are outcome-unknown: settle conservatively and
/// fence the task until `batch retry --resend-unknown ... --yes` authorizes one
/// fresh reservation. Explicit provider rejections are settled at zero and may
/// follow their normal backoff/auth recovery using a fresh reservation.
#[allow(clippy::too_many_arguments)]
fn record_sync_markdownize_failure(
    store: &TaskStore,
    ledger: &LedgerDb,
    key: &LedgerTaskKey,
    intent_token: &str,
    reserved_usd: f64,
    task_id: &str,
    task_attempts: u32,
    error: &TaskExecutionFailure,
    counts: &mut ExecOutcome,
) -> Result<()> {
    match error.retry_kind {
        RetryErrorKind::AuthError | RetryErrorKind::RateLimit | RetryErrorKind::QuotaExceeded => {
            settle_sync_definitive_rejection(ledger, key, intent_token, error.retry_kind)?;
            let policy = retry_policy(error.retry_kind);
            let attempts_after = task_attempts.saturating_add(1);
            let next_retry_at = (policy.retryable
                && policy
                    .max_attempts
                    .map(|max| attempts_after < max)
                    .unwrap_or(true))
            .then(|| {
                scheduled_retry_at(
                    &ledger.control().now_utc_seconds(),
                    &policy.backoff,
                    attempts_after,
                )
            });
            let reason = retry_reason(error.retry_kind).to_owned();
            store
                .update_matching(|task| {
                    if task.task_id != task_id {
                        return false;
                    }
                    task.clear_reservation();
                    task.heartbeat_at = None;
                    task.hold_reason = None;
                    task.fallback_reason = Some(reason.clone());
                    match error.retry_kind {
                        RetryErrorKind::AuthError => {
                            task.status = TaskStatus::Paused;
                            task.hold_reason = Some(HoldReason::Auth);
                            task.next_retry_at = None;
                        }
                        RetryErrorKind::RateLimit => {
                            task.status = TaskStatus::Pending;
                            task.next_retry_at = Some(rate_limit_retry_at(
                                &ledger.control().now_utc_seconds(),
                                error.retry_after_ms,
                            ));
                        }
                        RetryErrorKind::QuotaExceeded => {
                            task.status = TaskStatus::Failed;
                            task.attempts = attempts_after;
                            task.next_retry_at = next_retry_at.clone();
                        }
                        _ => unreachable!("definitive sync rejection was matched above"),
                    }
                    true
                })
                .map_err(pipeline_to_kio)?;
            counts.failed += 1;
            match error.retry_kind {
                RetryErrorKind::AuthError => counts.auth_failed += 1,
                RetryErrorKind::RateLimit | RetryErrorKind::QuotaExceeded
                    if next_retry_at.is_some() || error.retry_kind == RetryErrorKind::RateLimit =>
                {
                    counts.failed_retryable += 1;
                }
                _ => {}
            }
        }
        _ => {
            settle_task_charge_unknown(ledger, key, intent_token, reserved_usd)?;
            mark_sync_result_unknown_task(store, task_id)?;
            counts.failed += 1;
        }
    }
    Ok(())
}

/// QA2/QA3 (step4b-contract-tests-p3a.md §A, 04 §5.2/§5.3): the shared
/// send-failure mapping — a 3-way branch on `error.retry_kind`. rate_limit
/// and auth_error each get a dedicated non-Failed landing state; every other
/// kind keeps the Failed/backoff logic. Extracted from the sync send site
/// verbatim so the Batch submit lane joins the SAME classification
/// (04 §5.8 送信途中の Adapter エラー).
///
/// `settle_on_terminal`: `true` for the sync lane (unchanged behavior — a
/// definitive failure settles `unknown_settled` at the reservation estimate);
/// `false` for the Batch lane, whose phases 2a/2b records must stay open as
/// the §5.8 crash window a later recovery (poll / `kio ledger reconcile` /
/// abandon) reconciles — settling here would clear `intent_token` while
/// provider residue (upload/job) may still exist.
#[allow(clippy::too_many_arguments)]
fn record_online_send_failure(
    store: &TaskStore,
    ledger: &LedgerDb,
    key: &LedgerTaskKey,
    intent_token: &str,
    reserved_usd: f64,
    task_id: &str,
    task_attempts: u32,
    error: &TaskExecutionFailure,
    counts: &mut ExecOutcome,
    settle_on_terminal: bool,
) -> Result<()> {
    match error.retry_kind {
        RetryErrorKind::RateLimit => {
            // QA3 (04 §5.2 L682-683): "paused ではなく pending +
            // next_retry_at" — stays Pending, never Failed. `attempts`
            // is NOT consumed (max_attempts=∞, 04 §5.3). CL42/CL44/CL45:
            // a retry is coming — leave the ledger row open (same
            // rationale as the settles_now=false case below); the
            // reservation stamp is left untouched.
            let next_retry_at =
                rate_limit_retry_at(&ledger.control().now_utc_seconds(), error.retry_after_ms);
            let reason = retry_reason(RetryErrorKind::RateLimit).to_owned();
            store
                .update_matching(|candidate| {
                    if candidate.task_id == task_id {
                        candidate.status = TaskStatus::Pending;
                        candidate.fallback_reason = Some(reason.clone());
                        candidate.hold_reason = None;
                        candidate.heartbeat_at = None;
                        candidate.next_retry_at = Some(next_retry_at.clone());
                        true
                    } else {
                        false
                    }
                })
                .map_err(pipeline_to_kio)?;
            // R9-7: a driven task that failed still attempted an online
            // send — count it so `batch` JSON surfaces the attempt.
            counts.failed += 1;
            // R11-2/04 §5.6 exit 3: retryable work remains.
            counts.failed_retryable += 1;
        }
        RetryErrorKind::AuthError => {
            // QA2 (04 §5.2 L679-683): lands Paused(hold_reason=auth),
            // never Failed. `attempts` is NOT consumed (a pause is not
            // a retry-budget event — max_attempts=0 means "no retry",
            // not "budget already spent"), so a later revival (`batch
            // resume`) does not find the budget exhausted. Ledger:
            // EXACTLY the pre-QA2 definitive-failure behavior (settle
            // `unknown_settled` + clear the reservation) — a later
            // revival reserves fresh. Batch lane: no settle — the open
            // row (and any 2a/2b residue) belongs to §5.8 recovery.
            if settle_on_terminal {
                settle_task_charge_unknown(ledger, key, intent_token, reserved_usd)?;
            }
            let reason = retry_reason(RetryErrorKind::AuthError).to_owned();
            store
                .update_matching(|candidate| {
                    if candidate.task_id == task_id {
                        candidate.status = TaskStatus::Paused;
                        candidate.hold_reason = Some(HoldReason::Auth);
                        candidate.fallback_reason = Some(reason.clone());
                        candidate.heartbeat_at = None;
                        candidate.next_retry_at = None;
                        candidate.clear_reservation();
                        true
                    } else {
                        false
                    }
                })
                .map_err(pipeline_to_kio)?;
            counts.failed += 1;
            // R11-2/04 §5.6 exit 5: auth_error needs user action.
            counts.auth_failed += 1;
        }
        _ => {
            let policy = retry_policy(error.retry_kind);
            let attempts_after = task_attempts.saturating_add(1);
            let next_retry_at = (policy.retryable
                && policy
                    .max_attempts
                    .map(|max| attempts_after < max)
                    .unwrap_or(true))
            .then(|| {
                scheduled_retry_at(
                    &ledger.control().now_utc_seconds(),
                    &policy.backoff,
                    attempts_after,
                )
            });
            // CL42/CL44/CL45: a retry is coming — leave the ledger row open (it
            // already covers the resend, `reserve_or_reuse_task_charge`'s reuse
            // path) rather than settling now. Only a definitive outcome (no more
            // retries left) settles `unknown_settled` at the reservation estimate
            // — this Adapter integration has no post-hoc result-query capability
            // (see `settle_task_charge_unknown`'s doc comment), so a permanently
            // failed send is never billed less than its reservation. Batch lane:
            // never settles here (see `settle_on_terminal`'s doc).
            let settles_now = settle_on_terminal && next_retry_at.is_none();
            if settles_now {
                settle_task_charge_unknown(ledger, key, intent_token, reserved_usd)?;
            }
            let reason = retry_reason(error.retry_kind).to_owned();
            store
                .update_matching(|candidate| {
                    if candidate.task_id == task_id {
                        candidate.status = TaskStatus::Failed;
                        candidate.fallback_reason = Some(reason.clone());
                        // A Failed task is not a hold.
                        candidate.hold_reason = None;
                        candidate.heartbeat_at = None;
                        candidate.attempts = candidate.attempts.saturating_add(1);
                        candidate.next_retry_at = next_retry_at.clone();
                        if settles_now {
                            candidate.clear_reservation();
                        }
                        true
                    } else {
                        false
                    }
                })
                .map_err(pipeline_to_kio)?;
            // R9-7: a driven task that failed still attempted an online send
            // (charge consumed) — count it so `batch` JSON surfaces the
            // attempt instead of reporting only successes.
            counts.failed += 1;
            // R11-2: a scheduled `next_retry_at` marks the task retry-eligible
            // (exit 3), else it counts toward all-permanent (exit 4).
            if next_retry_at.is_some() {
                counts.failed_retryable += 1;
            }
        }
    }
    Ok(())
}

fn persistent_network_allowed(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
) -> Result<bool> {
    let profile = match active_local_ocr_execution_with_control(adapter_settings, repo.control()) {
        Some(execution) => kio_adapter::local_ocr_markdownize::profile_for(execution),
        None => online_markdownize_profile_for(repo)?,
    };
    persistent_network_allowed_for(
        adapter_settings,
        repo,
        approvals::AdapterRole::Markdown,
        &profile.adapter_id,
        &profile.tool_profile_hash,
    )
}

fn persistent_network_allowed_for(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    role: approvals::AdapterRole,
    tool_id: &str,
    profile: &str,
) -> Result<bool> {
    approvals::allowed(
        adapter_settings,
        repo,
        role,
        tool_id,
        profile,
        grants::GrantOperation::Network,
    )
}

fn local_markdownize_transport_allowed(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    profile: &AdapterProfile,
    needs_secret: bool,
) -> Result<bool> {
    current_embedding_policy(repo)?
        .revalidate()
        .map_err(pipeline_to_kio)?;
    if !persistent_network_allowed_for(
        adapter_settings,
        repo,
        approvals::AdapterRole::Markdown,
        &profile.adapter_id,
        &profile.tool_profile_hash,
    )? {
        return Ok(false);
    }
    Ok(!needs_secret
        || secrets_send_approved(
            adapter_settings,
            repo,
            approvals::AdapterRole::Markdown,
            &profile.adapter_id,
            &profile.tool_profile_hash,
        ))
}

/// Admission at the last safe boundary before a non-deterministic embedding
/// transport. The caller chooses whether it is before a paid reservation
/// (online) or directly at an unmetered local HTTP call.
fn admit_embedding_transport(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    execution: EmbeddingExecution,
    profile: &DeclaredEmbeddingProfile,
    current_policy: &CurrentPolicyEvaluator,
    needs_secret: bool,
) -> Result<()> {
    if embedding_is_deterministic(execution) {
        return Ok(());
    }
    current_policy.revalidate().map_err(pipeline_to_kio)?;
    if !persistent_network_allowed_for(
        adapter_settings,
        repo,
        approvals::AdapterRole::Embedding,
        &profile.tool_id,
        &profile.profile_hash,
    )? {
        return Err(KioError::new(
            "KIO-E-ADAPTER-APPROVAL-REQUIRED-001",
            "embedding network approval is no longer current",
            json!({}),
            ExitCode::AuthError,
        ));
    }
    if needs_secret && !embedding_secrets_allowed(adapter_settings, repo, profile)? {
        return Err(KioError::new(
            "KIO-E-ADAPTER-APPROVAL-REQUIRED-001",
            "embedding secret-send approval is no longer current",
            json!({}),
            ExitCode::AuthError,
        ));
    }
    Ok(())
}

fn embedding_secrets_allowed(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    profile: &DeclaredEmbeddingProfile,
) -> Result<bool> {
    approvals::allowed(
        adapter_settings,
        repo,
        approvals::AdapterRole::Embedding,
        &profile.tool_id,
        &profile.profile_hash,
        grants::GrantOperation::SendSecrets,
    )
}

fn persistent_network_allowed_for_kio_dir(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    kio_dir: &Path,
    role: approvals::AdapterRole,
    tool_id: &str,
    profile: &str,
    control: &kio_core::control::ControlHandle,
) -> Result<bool> {
    approvals::allowed_at(
        adapter_settings,
        kio_dir,
        role,
        tool_id,
        profile,
        grants::GrantOperation::Network,
        control,
    )
}

/// Query text is supplied by this invocation. One currently authorized eligible
/// scope may admit its single embedding request; --online does not create a grant.
fn embedding_opt_in_for_scopes(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    exec_scopes: &[ExecScope],
    tool_id: &str,
    tool_profile_hash: &str,
    offline: bool,
    control: &kio_core::control::ControlHandle,
) -> Result<bool> {
    if offline {
        return Ok(false);
    }
    for exec in exec_scopes {
        if persistent_network_allowed_for_kio_dir(
            adapter_settings,
            &exec.target.kio_dir,
            approvals::AdapterRole::Embedding,
            tool_id,
            tool_profile_hash,
            control,
        )? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn embedding_online_allowed(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    offline: bool,
    _online: bool,
    _online_confirmed: bool,
) -> Result<bool> {
    let Some(execution) = embedding_execution(adapter_settings, repo.control()) else {
        return Ok(false);
    };
    if embedding_is_deterministic(execution) {
        return Ok(true);
    }
    if !embedding_transport_allowed(execution, offline) {
        return Ok(false);
    }
    let profile = declared_embedding_profile(execution);
    persistent_network_allowed_for(
        adapter_settings,
        repo,
        approvals::AdapterRole::Embedding,
        &profile.tool_id,
        &profile.profile_hash,
    )
}

/// QA21/23: `active_embedding_adapter_id()` plus the adapter's CURRENT
/// `tool_profile_hash` (needed for the `approvals[]` profile match). `None`
/// when no embedding execution is active, mirroring
/// `active_embedding_adapter_id`.
fn active_embedding_adapter_identity(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    control: &ControlHandle,
) -> Result<Option<(String, String)>> {
    let Some(execution) = embedding_execution(adapter_settings, control) else {
        return Ok(None);
    };
    let profile = declared_embedding_profile(execution);
    Ok(Some((profile.tool_id, profile.profile_hash)))
}

#[derive(Debug, Clone)]
struct OnlineExecutionOutcome {
    output_ref: String,
    status: TaskStatus,
    // R13-1: normalization-run provenance (docs/04:474-479) so the caller records
    // mode / previous_raw_hash / parent_run_id / changed_unit_keys on the task.
    // `Full` with empty/None for a full or unit-scoped-retry send.
    mode: MarkdownizeMode,
    previous_raw_hash: Option<String>,
    parent_run_id: Option<String>,
    changed_unit_keys: Vec<String>,
    /// QA17 (step4b-contract-tests-p3a.md §F): the adapter's self-reported
    /// billing usage for the send that produced this outcome (07 §4),
    /// forwarded from the `MarkdownizeResponse` the adapter returned. `None`
    /// when nothing was sent (a 0-change incremental) or the concrete
    /// adapter has no real per-call signal to report — the settle call site
    /// degrades to the reservation estimate in that case, unchanged from
    /// before this field existed.
    usage: Option<kio_adapter::types::AdapterUsage>,
}

impl OnlineExecutionOutcome {
    /// A `Full`-mode outcome (full send or unit-scoped retry) with no incremental
    /// provenance.
    fn full(output_ref: String, status: TaskStatus) -> Self {
        Self {
            output_ref,
            status,
            mode: MarkdownizeMode::Full,
            previous_raw_hash: None,
            parent_run_id: None,
            changed_unit_keys: Vec::new(),
            usage: None,
        }
    }

    /// QA17: attach the adapter-reported `usage` this send actually billed.
    fn with_usage(mut self, usage: Option<kio_adapter::types::AdapterUsage>) -> Self {
        self.usage = usage;
        self
    }
}

#[derive(Debug, Clone)]
struct TaskExecutionFailure {
    retry_kind: RetryErrorKind,
    // QA3 (step4b-contract-tests-p3a.md §A, 04 §5.3): the provider's
    // `Retry-After` in milliseconds, carried from `AdapterError::RateLimit`
    // through to the send handler so `next_retry_at` can honor it
    // (`rate_limit_retry_at`). `None` for every non-adapter construction
    // site and for an adapter error that is not `RateLimit` with a header.
    retry_after_ms: Option<u64>,
}

/// R11-6: the online-markdownize cost to reserve/bill for `task`. A full send bills
/// the whole document; a unit-scoped retry (`task.unit_keys` = the still-failed
/// units) bills only its share = full × (retried units / total prepared units),
/// with a floor of one unit's worth. `total` comes from the same verified buffer and
/// prepare result that is later sent, so charge and request cardinality cannot race.
fn prorated_markdownize_cost(
    task: &TaskDescriptor,
    file_size: u64,
    prepared_unit_count: usize,
    bbox_annotation_enabled: bool,
) -> f64 {
    let full = estimate_online_markdownize_cost(file_size, bbox_annotation_enabled);
    let Some(unit_keys) = task.unit_keys.as_ref().filter(|keys| !keys.is_empty()) else {
        return full;
    };
    if prepared_unit_count == 0 {
        return full;
    }
    let sent = unit_keys.len().min(prepared_unit_count).max(1);
    full * (sent as f64 / prepared_unit_count as f64)
}

/// R15-2 (+ R14-2 / R9-2): the NETWORK-FREE preconditions an online markdownize task
/// must satisfy before it is charged/executed. Returns `false` when the task can never
/// reach the adapter: the current file no longer hashes to `input_hash` (edited after
/// enqueue → superseded by R14-2 inside the executor), the file is text-native (R9-2
/// rejects it), or its input can't be prepared into any unit. In every case the adapter
/// is never called, so reserving the cost first (F8 charges before the send) would be a
/// phantom charge against a send that never happens — it double-bills and can exhaust the
/// markdownize cap, falsely pausing the valid task (R15-2). The executor re-checks the
/// same conditions as defense in depth; this is the loop's pre-charge gate.
/// R21-3: outcome of the send-time online-markdownize precondition check.
enum OnlineMarkdownizePrecondition {
    /// File present, unchanged, within the input cap, non-text-native, with locally
    /// extracted units to enhance — ready to send.
    Send(PreparedOnlineMarkdownizeInput),
    /// Genuine failure — retire the task (file gone / edited-since-enqueue / text-native /
    /// oversize). The recovery is a fresh re-index, not a retry.
    Retire,
    /// The file is live and recognized, but Kio does not yet have a bounded local
    /// conversion contract that can feed it to the OCR adapter. Keep it Pending without
    /// charging or sending; currently this is the Office-container path.
    AwaitConversion,
    /// 07 §5.1: an Office (DOCX/PPTX) file's runtime conversion (re-)attempt
    /// failed while re-preparing this task's precondition -- never reached
    /// the adapter, never billed. Distinct from `Retire`: this "joins"
    /// contract_violation semantics (04 §5.3, retryable once) rather than a
    /// permanent retirement, since a renderer hiccup is not the same kind of
    /// failure as a genuinely stale/edited-since-enqueue file.
    ConversionFailed,
}

struct PreparedOnlineMarkdownizeInput {
    path: PathBuf,
    media_type: String,
    bytes: Vec<u8>,
    prepared_units: Vec<PreparedUnit>,
}

/// R22-5: whether a locally-preparable input is really a TEXT file that only landed in
/// `application/octet-stream` because its extension is absent from the MIME table
/// (`.yaml`, `.json`, `.toml`, `.sh`, `Dockerfile`, …). `prepare_units` sniffs the bytes and
/// emits exactly one `File` unit for such input (`prepare.rs:90`), whereas a true binary
/// yields no units at all. R21-4 stopped *enqueueing* these for online OCR, but the send
/// gates still classified them by canonical MIME only — so a task an older build had already
/// enqueued survived the upgrade and `batch resume` shipped the raw bytes to the OCR API and
/// billed for it. This is the send-side mirror of R21-4's enqueue guard.
fn is_local_passthrough_text(media_type: &str, prepared_units: &[PreparedUnit]) -> bool {
    media_type == "application/octet-stream"
        && prepared_units.len() == 1
        && prepared_units[0].unit_type == UnitType::File
}

fn retry_unit_subset_is_valid(task: &TaskDescriptor, prepared_units: &[PreparedUnit]) -> bool {
    let Some(unit_keys) = &task.unit_keys else {
        return true;
    };
    if unit_keys.is_empty() {
        return false;
    }
    let requested = unit_keys
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if requested.len() != unit_keys.len() {
        return false;
    }
    let prepared = prepared_units
        .iter()
        .map(|unit| unit.unit_key.as_str())
        .collect::<BTreeSet<_>>();
    requested.is_subset(&prepared)
}

fn classify_online_markdownize_precondition(
    repo: &Repository,
    task: &TaskDescriptor,
) -> OnlineMarkdownizePrecondition {
    if raw_ingest_is_purge_blocked(repo, &task.input_hash) {
        return OnlineMarkdownizePrecondition::Retire;
    }
    let path = repository_input_display_path(repo, &task.input_path);
    let media_type = media_type_for_cli_path(&path).to_owned();
    if !repository_scan_policy_allows_file(repo, &task.input_path).unwrap_or(false) {
        return OnlineMarkdownizePrecondition::Retire;
    }
    let Ok(verified) =
        read_repository_scan_input(repo, &task.input_path, effective_max_input_bytes(repo))
    else {
        return OnlineMarkdownizePrecondition::Retire;
    };
    if verified.raw_hash != task.input_hash {
        return OnlineMarkdownizePrecondition::Retire;
    }
    // R19-8: honor a `max_input_bytes` cap that was tightened AFTER this task was
    // enqueued. The enqueue-time gate (run_index_pipeline) only checked the cap in effect
    // then; a Pending task must not be sent to the online adapter if the operator has
    // since lowered the cap below the file's size — the same live re-check the sibling
    // `[adapter.policy]` key `allow_network` already gets at send time.
    if is_text_native_media(&media_type) {
        return OnlineMarkdownizePrecondition::Retire;
    }
    let prepare_profile_hash = builtin_prepare_profile().tool_profile_hash;
    let input_path = path.display().to_string();
    let prepare = match prepare_units_from_bytes_with_control(
        PrepareStageBytesRequest {
            raw_hash: &task.input_hash,
            media_type: &media_type,
            input_path: &input_path,
            tool_profile_hash: &prepare_profile_hash,
            bytes: &verified.bytes,
        },
        repo.control(),
    ) {
        Ok(prepare) => prepare,
        Err(error) => {
            // 07 §5.1: a runtime Office conversion failure discovered while
            // re-preparing this task's precondition (the SAME converter
            // resolution + `convert_to_pdf` this file's earlier successful
            // enqueue-time prepare used, now failing -- a renderer hiccup,
            // the fixture/binary having gone away mid-run, etc.) "joins"
            // contract_violation semantics (04 §5.3: retryable, one retry)
            // rather than a permanent Retire -- a transient failure should
            // get one more chance, unlike a genuinely stale/edited-since-
            // enqueue file. Every OTHER prepare failure (a corrupt/oversized
            // file, an unrelated contract error) keeps today's Retire
            // behavior unchanged.
            let is_office_conversion_failure =
                pipeline_to_kio(error).error_code() == "KIO-E-PREPARE-OFFICE-CONVERT-001";
            return if is_office_conversion_failure {
                OnlineMarkdownizePrecondition::ConversionFailed
            } else {
                OnlineMarkdownizePrecondition::Retire
            };
        }
    };
    if prepare.prepared_units.is_empty() {
        // A supported non-text-native file reaches the provider without local unit
        // hints; the OCR response discovers its canonical page/image units.
        // A unit-scoped retry can recover those units from the previously-persisted
        // Partial manifest. Office containers still need a separate bounded conversion
        // contract, and unknown octet-stream binaries are never valid OCR inputs.
        if !supports_ocr_from_scratch(&media_type) {
            return if media_type == "application/octet-stream" {
                OnlineMarkdownizePrecondition::Retire
            } else {
                OnlineMarkdownizePrecondition::AwaitConversion
            };
        }
        let prepared_units = if task.unit_keys.is_some() {
            let store = TaskStore::new_with_control(repo.kio_dir(), repo.control().clone());
            let Ok(Some(previous)) = load_previous_instance_for_task(&store, task) else {
                return OnlineMarkdownizePrecondition::Retire;
            };
            previous.prepared_units
        } else {
            Vec::new()
        };
        if !retry_unit_subset_is_valid(task, &prepared_units) {
            return OnlineMarkdownizePrecondition::Retire;
        }
        return OnlineMarkdownizePrecondition::Send(PreparedOnlineMarkdownizeInput {
            path,
            media_type,
            bytes: verified.bytes,
            prepared_units,
        });
    }
    // R22-5: retire a stale task for octet-stream TEXT. The recovery is a re-index (which
    // no longer enqueues it), not a retry — the deterministic pass already handled the file.
    if is_local_passthrough_text(&media_type, &prepare.prepared_units) {
        return OnlineMarkdownizePrecondition::Retire;
    }
    if !retry_unit_subset_is_valid(task, &prepare.prepared_units) {
        return OnlineMarkdownizePrecondition::Retire;
    }
    OnlineMarkdownizePrecondition::Send(PreparedOnlineMarkdownizeInput {
        path,
        media_type,
        bytes: verified.bytes,
        prepared_units: prepare.prepared_units,
    })
}

/// Run every Pending local-HTTP markdownize task this pass.
///
/// Called from `kio index` rather than `kio batch resume`, because the two
/// commands are split by *approval and billing*. The task itself is kept — it
/// carries retry, partial-failure
/// recording, crash recovery, and an honest `pending_enrichment_tasks` — so
/// this shares the online task lifecycle while using a non-billable local
/// transport guarded by the same dispatch-time approval.
///
/// No ledger row is opened and none is settled. That is not an omission to be
/// checked for: there is no reservation to release either, so the failure path
/// cannot strand one the way the online lane's can.
fn execute_pending_offline_markdownize_tasks(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    store: &TaskStore,
    normalize_by_path: &mut BTreeMap<String, PendingNormalizeRef>,
    network_allowed: bool,
) -> Result<usize> {
    // An `offline_api` adapter can still be an authenticated local HTTPS peer.
    // `--offline` and an absent exact device+scope grant therefore leave the
    // task pending before constructing its client or touching the peer.
    if !network_allowed {
        return Ok(0);
    }
    let Some(execution) = active_local_ocr_execution_with_control(adapter_settings, repo.control())
    else {
        return Ok(0);
    };
    let profile = kio_adapter::local_ocr_markdownize::profile_for(execution);
    let output_ref = offline_output_ref(&profile.adapter_id);
    let mut executed = 0_usize;
    for task in store.all().map_err(pipeline_to_kio)? {
        if task.task_type != TaskType::Markdownize
            || task.status != TaskStatus::Pending
            || task.output_ref != output_ref
        {
            continue;
        }
        // Reused verbatim from the online lane, and worth reusing: it is what
        // refuses a task whose file was edited, purged, or pushed past the
        // input cap since enqueue. Persisting v2 bytes under v1's raw_hash
        // would break content-addressing identically on either route.
        let prepared_input = match classify_online_markdownize_precondition(repo, &task) {
            OnlineMarkdownizePrecondition::Send(prepared_input) => prepared_input,
            OnlineMarkdownizePrecondition::Retire => {
                retire_offline_markdownize_task(store, &task.task_id, "invalid_input")?;
                continue;
            }
            _ => continue,
        };
        // Revalidate this exact peer and the current scope policy immediately
        // before constructing its HTTP client. Other OCR or embedding grants
        // cannot authorize a local OCR secret send.
        if !local_markdownize_transport_allowed(
            adapter_settings,
            repo,
            &profile,
            classify_secret(&task.input_path).is_some(),
        )? {
            continue;
        }
        let task_id = task.task_id.clone();
        match execute_offline_markdownize_task(
            adapter_settings,
            repo,
            &task,
            prepared_input,
            execution,
        ) {
            Ok(outcome) => {
                // `persist_normalized_instance` has just committed the immutable
                // normalized-unit CAS objects.  Bind this pass's tree entry to the
                // manifest selected from that authoritative representation before
                // the later rebuild reads it.  The path-named normalized instance
                // remains a cache; using its old pre-OCR manifest pin here would
                // correctly make the CAS-aware rebuild see no units at all.
                let manifest_hash =
                    compute_manifest_hash(repo, &task.input_hash, &profile.tool_profile_hash, 0)?;
                normalize_by_path.insert(
                    task.input_path.clone(),
                    PendingNormalizeRef {
                        expected_raw_hash: task.input_hash.clone(),
                        normalize: NormalizeRef {
                            tool_profile_hash: profile.tool_profile_hash.clone(),
                            r#gen: 0,
                            manifest_hash,
                        },
                    },
                );
                store
                    .update_matching(|candidate| {
                        if candidate.task_id == task_id {
                            candidate.status = outcome.status;
                            candidate.output_ref = outcome.output_ref.clone();
                            candidate.fallback_reason = Some("local_adapter_done".to_owned());
                            candidate.heartbeat_at = None;
                            candidate.mode = Some(outcome.mode);
                            candidate.changed_unit_keys = outcome.changed_unit_keys.clone();
                            if matches!(outcome.status, TaskStatus::Partial | TaskStatus::Failed) {
                                candidate.attempts = candidate.attempts.saturating_add(1);
                            }
                            true
                        } else {
                            false
                        }
                    })
                    .map_err(pipeline_to_kio)?;
                executed += 1;
            }
            Err(failure) => {
                // Left Pending on a retryable failure — a model server that is
                // not up yet is the ordinary case, and the next `kio index`
                // should simply pick the task up again. Only a genuinely
                // non-retryable one is retired, so a stopped server never
                // silently discards work.
                let retryable = !matches!(
                    failure.retry_kind,
                    RetryErrorKind::InvalidInput | RetryErrorKind::ContractViolation
                );
                let reason = if retryable {
                    "local_adapter_unavailable"
                } else {
                    "contract_violation"
                };
                store
                    .update_matching(|candidate| {
                        if candidate.task_id == task_id {
                            candidate.attempts = candidate.attempts.saturating_add(1);
                            candidate.fallback_reason = Some(reason.to_owned());
                            if !retryable {
                                candidate.status = TaskStatus::Failed;
                            }
                            true
                        } else {
                            false
                        }
                    })
                    .map_err(pipeline_to_kio)?;
            }
        }
    }
    Ok(executed)
}

fn retire_offline_markdownize_task(store: &TaskStore, task_id: &str, reason: &str) -> Result<()> {
    store
        .update_matching(|candidate| {
            if candidate.task_id == task_id {
                candidate.status = TaskStatus::Failed;
                candidate.fallback_reason = Some(reason.to_owned());
                candidate.heartbeat_at = None;
                true
            } else {
                false
            }
        })
        .map_err(pipeline_to_kio)?;
    Ok(())
}

/// One document through the local pipeline.
///
/// Always a Full send. The incremental path the online lane tries first exists
/// to avoid re-billing pages that did not change; with nothing to bill, its
/// only remaining effect would be extra ways to be wrong about page
/// correspondence.
fn execute_offline_markdownize_task(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    task: &TaskDescriptor,
    prepared_input: PreparedOnlineMarkdownizeInput,
    execution: LocalOcrExecution,
) -> std::result::Result<OnlineExecutionOutcome, TaskExecutionFailure> {
    let PreparedOnlineMarkdownizeInput {
        media_type,
        bytes,
        prepared_units,
        ..
    } = prepared_input;
    let profile = kio_adapter::local_ocr_markdownize::profile_for(execution);
    let scope_id = repo
        .scope_identity()
        .map(|identity| identity.scope_id)
        .map_err(|_| TaskExecutionFailure {
            retry_kind: RetryErrorKind::InvalidInput,
            retry_after_ms: None,
        })?;
    let adapter = local_ocr_markdownize_adapter_with_control(
        adapter_settings,
        execution,
        &scope_id,
        repo.kio_dir(),
        &bytes,
        repo.control(),
    )
    .map_err(task_failure_from_adapter)?;
    let hints = prepared_unit_hints(&prepared_units);
    let response = adapter
        .markdownize(MarkdownizeRequest {
            raw: RawInput {
                raw_hash: task.input_hash.clone(),
                path: Some(task.input_path.clone()),
            },
            media_type: media_type.clone(),
            prepared_unit_hint: Some(hints.clone()),
            mode: AdapterMarkdownizeMode::Full,
            previous: None,
            hints: None,
            restrict_to_hint_pages: false,
            // 07 §5.2's bbox_annotation is Mistral's prompt-and-charge feature;
            // the local adapter refuses the flag rather than pretending.
            bbox_annotation_enabled: false,
            tool_profile_hash: profile.tool_profile_hash.clone(),
            spec_version: 1,
            // No ledger row exists, so there is no intent token to carry.
            idempotency_token: None,
        })
        .map_err(task_failure_from_adapter)?;
    // OCR-from-scratch is the normal case here: a scanned PDF has no text layer,
    // so `prepared_units` is empty and the unit identities exist only in the
    // response. Passing the empty request set straight through made every such
    // document persist as zero units and fail the contract check.
    let effective_hints = effective_prepared_unit_hints(&hints, &task.input_hash, &response)
        .map_err(task_failure_from_adapter)?;
    materialize_online_markdownize_response(
        repo,
        task,
        prepared_units,
        &media_type,
        &bytes,
        None,
        profile.tool_profile_hash,
        response,
        &effective_hints,
    )
}

fn execute_online_markdownize_task(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    task: &TaskDescriptor,
    prepared_input: PreparedOnlineMarkdownizeInput,
    // QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880): the caller's
    // ledger `intent_token` for this send, threaded into every
    // `MarkdownizeRequest` this call builds (both the incremental and Full
    // paths below) — see `MarkdownizeRequest::idempotency_token`'s doc.
    idempotency_token: Option<String>,
) -> std::result::Result<OnlineExecutionOutcome, TaskExecutionFailure> {
    let PreparedOnlineMarkdownizeInput {
        path,
        media_type,
        bytes,
        prepared_units,
    } = prepared_input;
    // R14-2: an online markdownize task is always deferred — one pass enqueues it and
    // a later `batch resume` / `index --online` executes it — so the file may have been
    // edited in between. Executing a stale task reads the CURRENT bytes yet persists
    // them under the enqueue-time `input_hash` (identity = raw_hash), i.e. it stores v2
    // content under v1 identity. That breaks the content-addressing invariant, is
    // sticky (CAS idempotency never re-OCRs H(v1) again), mis-bills the v2 OCR under
    // v1, and poisons the next incremental's baseline (`latest_online_instance_for_path`
    // reads the tainted instance as "previous"). If the current file no longer hashes
    // to `task.input_hash`, supersede this task: do NOT execute or persist. The next
    // `index` enqueues a fresh task for the new content (the recovery is a re-index, not
    // a retry of this obsolete task — hence non-retryable InvalidInput; the task state
    // machine has no dedicated "superseded" state). Distinct from R14-1 (which degrades
    // an unreadable PREVIOUS instance to a Full re-send of the CURRENT content): here the
    // CURRENT content itself no longer matches the task, so there is nothing to send.
    if !repository_scan_policy_allows_file(repo, &task.input_path).unwrap_or(false) {
        return Err(TaskExecutionFailure {
            retry_kind: RetryErrorKind::InvalidInput,
            retry_after_ms: None,
        });
    }
    // R9-2 (defense in depth): never send a text-native file to online OCR even if
    // a task for it already exists (e.g. enqueued by a pre-fix build or a poisoned
    // tasks.jsonl). Fail fast as a non-retryable input error instead of a silent,
    // billed send.
    if is_text_native_media(&media_type) {
        return Err(TaskExecutionFailure {
            retry_kind: RetryErrorKind::InvalidInput,
            retry_after_ms: None,
        });
    }
    // R22-5 (defense in depth, mirroring the R9-2 guard above): never ship octet-stream TEXT
    // to online OCR even if a task for it exists (enqueued by a pre-R21-4 build, or a
    // poisoned tasks.jsonl). Fail fast as a non-retryable input error, never a billed send.
    if is_local_passthrough_text(&media_type, &prepared_units) {
        return Err(TaskExecutionFailure {
            retry_kind: RetryErrorKind::InvalidInput,
            retry_after_ms: None,
        });
    }
    let scope_id = repo
        .scope_identity()
        .map(|identity| identity.scope_id)
        .map_err(|_| TaskExecutionFailure {
            retry_kind: RetryErrorKind::InvalidInput,
            retry_after_ms: None,
        })?;
    // R11-6: on a UNIT-SCOPED retry, `task.unit_keys` names the still-failed units.
    // Request ONLY those from the adapter (re-OCR + re-bill just the failed subset,
    // not the whole document); the full prepared set still drives the manifest so
    // done/failed accounting covers every unit. `unit_keys == None` = a full send.
    let retry_units: Option<BTreeSet<String>> = task
        .unit_keys
        .as_ref()
        .map(|keys| keys.iter().cloned().collect());
    // R13-1: for a FRESH send (not a unit-scoped retry), try incremental Markdownize
    // first — re-OCR only the changed/added pages and reuse the unchanged pages'
    // markdown from the prior online instance (docs/04 §2.2/§3.1, docs/07 §8 note).
    // Returns Some on success; None when it doesn't apply or the acceptance check
    // fails (fall through to the Full send below); Err on an adapter auth/rate error
    // (a Full re-send would hit the same error, so propagate).
    //
    // 07 §5.1 scope-trim: Office (DOCX/PPTX) media always sends `mode=Full`. The
    // prior online instance's `PreviousMarkdownizeContext.raw.raw_hash` (built
    // from `previous.manifest.raw_hash`, the ORIGINAL file's raw_hash) has no
    // sound relationship to a fresh per-send CONVERTED-PDF identity
    // (`run_standard_online_markdownize_with_bytes` computes a NEW hash from a
    // freshly reconverted PDF on every call, §5.1's converted-PDF path) --
    // reusing it for an incremental "diff against the previous document" send
    // would need the OCR adapter/provider to understand two DIFFERENT converted
    // PDFs' page correspondence, which nothing in this contract establishes.
    // Full re-markdownize is correct and safe; see the implementation report for
    // the fuller incremental-office design space this defers.
    if retry_units.is_none()
        && task.fallback_reason.as_deref() != Some(SYNC_RESEND_AUTHORIZED_REASON)
        && !prepared_units.is_empty()
        && !is_office_media(&media_type)
        && let Some(outcome) = try_online_incremental_markdownize(
            adapter_settings,
            repo,
            task,
            &prepared_units,
            &scope_id,
            &media_type,
            &path,
            &bytes,
            idempotency_token.clone(),
        )?
    {
        return Ok(outcome);
    }
    let request_units: Vec<PreparedUnit> = match &retry_units {
        Some(keys) => prepared_units
            .iter()
            .filter(|unit| keys.contains(&unit.unit_key))
            .cloned()
            .collect(),
        None => prepared_units.clone(),
    };
    let outcome = run_standard_online_markdownize_with_bytes_with_control(
        std::sync::Arc::clone(adapter_settings),
        StandardOnlineMarkdownizeRequest {
            scope_id: &scope_id,
            kio_dir: repo.kio_dir(),
            raw_hash: &task.input_hash,
            path: &path,
            media_type: &media_type,
            prepared_unit_hints: prepared_unit_hints(&request_units),
            // Full send (or unit-scoped retry): every requested unit, no previous/hints.
            mode: AdapterMarkdownizeMode::Full,
            previous: None,
            hints: None,
            // R15-5: a unit-scoped retry (`retry_units.is_some()`) re-sends ONLY the failed
            // subset — scope the real OCR send/bill to those pages instead of the whole
            // document (the ledger already prorated the reserve to the subset). A fresh full
            // send (`retry_units.is_none()`) leaves this false → whole document, no `pages`.
            restrict_to_hint_pages: retry_units.is_some(),
            bbox_annotation_enabled: task.bbox_annotation_enabled.unwrap_or(false),
            // QA13 (04 §5.5 L880): see this function's `idempotency_token`
            // param doc.
            idempotency_token,
        },
        &bytes,
        repo.control(),
    )
    .map_err(task_failure_from_adapter)?;
    materialize_online_markdownize_response(
        repo,
        task,
        prepared_units,
        &media_type,
        &bytes,
        retry_units.as_ref(),
        outcome.profile.tool_profile_hash.clone(),
        outcome.response,
        &outcome.effective_prepared_unit_hints,
    )
}

/// The response half of [`execute_online_markdownize_task`] — everything that
/// happens AFTER a `MarkdownizeResponse` is in hand: OCR-from-scratch unit
/// discovery, first-instance-wins merge for a unit-scoped retry, unit
/// validation, and manifest + normalized-instance persistence. Split out so
/// the Batch collect lane (04 §5.8 相 3 — `poll_batch_markdownize_jobs`)
/// drives the EXACT same verification/materialization path as the sync lane:
/// the two lanes differ only in where the response came from (a synchronous
/// POST vs a fetched batch-output line).
#[allow(clippy::too_many_arguments)]
fn materialize_online_markdownize_response(
    repo: &Repository,
    task: &TaskDescriptor,
    mut prepared_units: Vec<PreparedUnit>,
    media_type: &str,
    bytes: &[u8],
    retry_units: Option<&BTreeSet<String>>,
    profile_tool_hash: String,
    response: kio_adapter::types::MarkdownizeResponse,
    effective_prepared_unit_hints: &[PreparedUnitHint],
) -> std::result::Result<OnlineExecutionOutcome, TaskExecutionFailure> {
    if prepared_units.is_empty() {
        if retry_units.is_some() {
            return Err(TaskExecutionFailure {
                retry_kind: RetryErrorKind::ContractViolation,
                retry_after_ms: None,
            });
        }
        prepared_units = prepared_units_from_ocr_discovery(
            effective_prepared_unit_hints,
            media_type,
            &task.input_hash,
            bytes,
            &builtin_prepare_profile().tool_profile_hash,
        )
        .map_err(|_| TaskExecutionFailure {
            retry_kind: RetryErrorKind::ContractViolation,
            retry_after_ms: None,
        })?;
        // No page artifact exists before OCR for a scanned PDF. The already-verified
        // immutable raw object is the bounded prepared source shared by each discovered
        // unit; publish it under the same content hash before the manifest can refer to it.
        write_canonical_cas_object_or_reuse(repo.kio_dir(), "prepared", &task.input_hash, bytes)
            .map_err(|_| TaskExecutionFailure {
                retry_kind: persist_failure_retry_kind(),
                retry_after_ms: None,
            })?;
    }
    // QA17 (step4b-contract-tests-p3a.md §F): carry the adapter's
    // self-reported billing usage (Mistral OCR's real processed-page count)
    // through to the caller's settle call, instead of discarding it here.
    let usage = response.usage.clone();
    // A unit-scoped retry requested only its failed subset. Validate exactly
    // that response boundary; immutable previously completed units are merged
    // below and must not be requested or billed again just to pass coverage.
    let response_prepared_units = prepared_units
        .iter()
        .filter(|unit| retry_units.is_none_or(|keys| keys.contains(&unit.unit_key)))
        .cloned()
        .collect::<Vec<_>>();
    let hints = all_changed_hints(&response_prepared_units);
    let acceptance = validate_markdownize_response(&response, &hints, &response_prepared_units);
    // Normalized Markdown is immutable once published. Every origin, including
    // paid sync and collected Batch output, must pass the same v1 acceptance gate
    // before any normalized CAS object is written. A fallback control response is
    // not archival content and is rejected here as well.
    let strict_valid = matches!(
        acceptance,
        Ok(kio_pipeline::markdownize::MarkdownizeAcceptance::Accepted)
    );
    if !strict_valid {
        return Err(TaskExecutionFailure {
            retry_kind: RetryErrorKind::ContractViolation,
            retry_after_ms: None,
        });
    }
    let generated_at = repo.control().now_utc_seconds();
    // R11-6: preserve previously-done units (first-instance-wins). Load the prior
    // instance this run overwrites (same raw_hash + resolved tool_profile_hash +
    // gen 0). Regenerating a done unit under Markdown non-determinism would churn its
    // fingerprint → needless re-embedding + Evidence churn (docs/04 §5.2).
    let previous = if retry_units.is_some() {
        load_previous_instance_identity(
            repo.kio_dir(),
            &task.input_hash,
            &profile_tool_hash,
            0,
            repo.control(),
        )
        .ok()
    } else {
        None
    };
    let mut units = normalized_units_from_response(
        repo,
        media_type,
        bytes,
        &response,
        &prepared_units,
        previous.as_ref(),
        &task.input_hash,
        &profile_tool_hash,
        0,
        MarkdownizeMode::Full,
        &generated_at,
    )
    .map_err(NormalizedUnitsError::task_failure)?;
    // R11-6: merge in the previously-done units the retry did not target. The retry
    // request omitted them, so the adapter never returned them — keep the FIRST
    // instance's output verbatim (first-instance-wins).
    if let Some(prev) = &previous {
        let produced: BTreeSet<String> = units.iter().map(|unit| unit.unit_key.clone()).collect();
        for prev_unit in &prev.units {
            if !produced.contains(&prev_unit.unit_key) {
                units.push(prev_unit.clone());
            }
        }
        // Re-establish document order after appending the preserved units.
        let order_of: BTreeMap<&str, u64> = prepared_units
            .iter()
            .map(|unit| (unit.unit_key.as_str(), unit.order))
            .collect();
        units.sort_by_key(|unit| {
            order_of
                .get(unit.unit_key.as_str())
                .copied()
                .unwrap_or(u64::MAX)
        });
    }
    // A validated provider response may legally report every requested unit in
    // `failed_units`. It is a known result, not a transport failure: publish the
    // zero-object manifest with every prepared unit marked Failed so sync settles
    // the known response and Batch collection cannot loop by re-fetching it.
    let done = units.len();
    let failed = prepared_units.len().saturating_sub(done);
    // Validation accepts well-formed failed_units as well as successful units.
    // Completion therefore depends on the merged output count, not acceptance.
    let status = task_status_from_unit_counts(done, failed, false);
    let run_id = format!("run_{}", new_ulid(repo.canonical_root()));
    let failed_kinds = merged_failed_unit_retry_kinds(&response, previous.as_ref(), retry_units);
    let manifest = manifest_from_units_with_failed_kinds(
        &prepared_units,
        &units,
        &task.input_hash,
        &profile_tool_hash,
        0,
        None,
        &run_id,
        &generated_at,
        &failed_kinds,
        RetryErrorKind::NetworkError,
    );
    let _stamped_manifest =
        persist_normalized_instance_with_control(repo.kio_dir(), &manifest, &units, repo.control())
            .map_err(|_| TaskExecutionFailure {
                retry_kind: persist_failure_retry_kind(),
                retry_after_ms: None,
            })?;
    Ok(OnlineExecutionOutcome::full(
        normalized_output_ref(repo, &task.input_hash, &profile_tool_hash, 0),
        status,
    )
    .with_usage(usage))
}

/// R13-1: attempt incremental Markdownize on the online (Mistral OCR) route.
/// `Ok(Some(outcome))` = incremental fired and passed the KIO-side acceptance
/// check (docs/04 §3.2). `Ok(None)` means the incremental route was rejected
/// before any provider request, so the caller may start one Full request. Once an
/// incremental request has been sent, every invalid or unpersistable response is
/// returned as `Err`; a non-idempotent provider call must never be followed by an
/// implicit second Full request under the same reservation. Only the changed+added pages are sent to the
/// API; unchanged pages are reused from the prior instance (`reused_from`), which
/// is the cost fix (a light revision no longer re-sends/re-bills every page).
#[allow(clippy::too_many_arguments)]
fn try_online_incremental_markdownize(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    task: &TaskDescriptor,
    prepared_units: &[PreparedUnit],
    scope_id: &str,
    media_type: &str,
    path: &Path,
    verified_raw_bytes: &[u8],
    // QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880): forwarded from
    // `execute_online_markdownize_task` — see its doc.
    idempotency_token: Option<String>,
) -> std::result::Result<Option<OnlineExecutionOutcome>, TaskExecutionFailure> {
    let invalid = || TaskExecutionFailure {
        retry_kind: RetryErrorKind::InvalidInput,
        retry_after_ms: None,
    };
    let incremental_config = effective_incremental_config(repo).map_err(|_| invalid())?;
    if !incremental_config.enabled {
        return Ok(None);
    }
    let task_store = TaskStore::new_with_control(repo.kio_dir(), repo.control().clone());
    // The prior online instance for this path (its own resolved tool_profile_hash).
    let Some(previous) =
        latest_online_instance_for_path(&task_store, &task.input_path).map_err(|_| invalid())?
    else {
        return Ok(None);
    };
    // unit_mapping (docs/04 §2.2) between the prior and current prepared units, then
    // the documented 5-condition decision (R12-1's `choose_markdownize_mode`, which
    // now reaches the incremental gate because the adapter declares the capability).
    let mapping = map_units(&previous.prepared_units, prepared_units);
    // R14-6: resolve the online adapter's profile NOW (this resolves the model pin — a
    // `GET /v1/models` for a `*-latest` alias, never an OCR upload/bill) so the pin/profile
    // gate below runs BEFORE any incremental send. Previously the mismatch was only checked
    // AFTER the incremental request had been sent (and, post-R14-4, the whole document
    // uploaded), wasting a send + bill and then re-sending Full. `capability_flags` are
    // pin-independent, so the mode decision is unchanged.
    let resolved_profile = resolve_standard_online_markdownize_profile_with_bbox_with_control(
        std::sync::Arc::clone(adapter_settings),
        scope_id,
        task.bbox_annotation_enabled.unwrap_or(false),
        repo.control(),
    )
    .map_err(task_failure_from_adapter)?;
    let decision = choose_markdownize_mode(&IncrementalModeInput {
        has_previous_done_run: true,
        raw_hash_only_changed: true,
        adapter_capabilities: resolved_profile.capability_flags.clone(),
        change_rate: mapping.change_rate,
        threshold: incremental_config.threshold,
        consecutive_incremental_count: consecutive_online_incremental_count(
            &task_store,
            &task.input_path,
        )
        .map_err(|_| invalid())?,
        max_consecutive_incremental: incremental_config.max_consecutive,
    });
    if decision.mode != MarkdownizeMode::Incremental {
        return Ok(None);
    }
    // R14-6 gate (docs/04 §3.1 condition 2): a changed resolved pin is a different
    // tool_profile → NOT an eligible incremental. Fall back to a Full send BEFORE sending
    // (and, post-R14-4, before uploading) anything. This replaces the former post-send
    // re-check.
    if resolved_profile.tool_profile_hash != previous.manifest.tool_profile_hash {
        return Ok(None);
    }
    let incremental_hints = incremental_hints_from_mapping(&mapping, prepared_units);
    // Send ONLY the changed+added units to the OCR API (the cost fix).
    let requested: Vec<PreparedUnit> = prepared_units
        .iter()
        .filter(|unit| {
            incremental_hints.changed_unit_keys.contains(&unit.unit_key)
                || incremental_hints.added_unit_keys.contains(&unit.unit_key)
        })
        .cloned()
        .collect();
    // R15-6: a 0-change incremental (`requested` empty — nothing changed or added; only
    // unchanged units, possibly with pure removals) must NOT touch the adapter. There is
    // no page to OCR, so reuse every current unit from the prior instance directly.
    // Before this, an empty `requested` still issued an Incremental request whose empty
    // `pages` the real Mistral client paired with the WHOLE base64 document (all-pages
    // OCR/bill), contradicting docs/04 §3.2 / 04-pipeline "unchanged units are reused
    // KIO-side, not sent". The resolved profile was gated equal to `previous` above, so
    // no send is needed to learn the tool_profile_hash.
    let incremental_sent = !requested.is_empty();
    let (mut response, profile_tool_hash) = if requested.is_empty() {
        (
            kio_adapter::types::MarkdownizeResponse {
                mode_used: AdapterMarkdownizeMode::Incremental,
                updated_units: Vec::new(),
                unchanged_unit_keys: Vec::new(),
                added_units: Vec::new(),
                removed_unit_keys: Vec::new(),
                failed_units: Vec::new(),
                fallback_to_full: false,
                reason: None,
                // QA17: a 0-change incremental never calls the adapter (see
                // comment above), so there is no real usage to report.
                usage: None,
            },
            resolved_profile.tool_profile_hash.clone(),
        )
    } else {
        let adapter_previous = PreviousMarkdownizeContext {
            raw: RawInput {
                raw_hash: previous.manifest.raw_hash.clone(),
                path: Some(path.display().to_string()),
            },
            normalized_units: previous
                .units
                .iter()
                .map(normalized_unit_to_adapter_unit)
                .collect(),
            tool_profile_hash: previous.manifest.tool_profile_hash.clone(),
        };
        let outcome = run_standard_online_markdownize_with_bytes_with_control(
            std::sync::Arc::clone(adapter_settings),
            StandardOnlineMarkdownizeRequest {
                scope_id,
                kio_dir: repo.kio_dir(),
                raw_hash: &task.input_hash,
                path,
                media_type,
                prepared_unit_hints: prepared_unit_hints(&requested),
                mode: AdapterMarkdownizeMode::Incremental,
                previous: Some(adapter_previous),
                hints: Some(adapter_hints(&incremental_hints)),
                // Incremental already scopes pages via `mode`; the retry-only signal stays off.
                restrict_to_hint_pages: false,
                bbox_annotation_enabled: task.bbox_annotation_enabled.unwrap_or(false),
                // QA13 (04 §5.5 L880): see this function's `idempotency_token`
                // param doc.
                idempotency_token,
            },
            verified_raw_bytes,
            repo.control(),
        )
        .map_err(task_failure_from_adapter)?;
        let response = outcome.response;
        // R14-6: the pin/profile mismatch was gated BEFORE the send above, so
        // `outcome.profile` matches `previous` here (same resolution) — no post-send
        // re-check is needed. Acceptance (docs/04 §3.2): the OCR response must cover
        // every requested (changed+added) unit, and the adapter must not have declined.
        // A response failure is after a non-idempotent call, so it cannot trigger
        // an implicit Full re-send.
        if response.fallback_to_full {
            return Err(TaskExecutionFailure {
                retry_kind: RetryErrorKind::ContractViolation,
                retry_after_ms: None,
            });
        }
        (response, outcome.profile.tool_profile_hash)
    };
    // Kio orchestrates the unchanged reuse (docs/07 §8: the document-processing
    // route reuses unchanged units KIO-side rather than via the adapter). Inject the
    // unchanged new keys so `normalized_units_from_response` copies their markdown
    // from the prior instance with `reused_from` set. In the 0-change path this is
    // EVERY current unit.
    response.unchanged_unit_keys = mapping
        .unchanged
        .iter()
        .map(|reuse| reuse.new_unit_key.clone())
        .collect();
    // The adapter response plus Kio-owned unchanged set is the exact v1
    // incremental partition. Validate it before a normalized unit can be
    // constructed; a bad post-send response must not trigger an implicit Full
    // request under the same non-idempotent reservation.
    if !matches!(
        validate_markdownize_response(&response, &incremental_hints, prepared_units),
        Ok(kio_pipeline::markdownize::MarkdownizeAcceptance::Accepted)
    ) {
        if !incremental_sent {
            return Ok(None);
        }
        return Err(TaskExecutionFailure {
            retry_kind: RetryErrorKind::ContractViolation,
            retry_after_ms: None,
        });
    }
    let generated_at = repo.control().now_utc_seconds();
    let units = match normalized_units_from_response(
        repo,
        media_type,
        verified_raw_bytes,
        &response,
        prepared_units,
        Some(&previous),
        &task.input_hash,
        &profile_tool_hash,
        0,
        MarkdownizeMode::Incremental,
        &generated_at,
    ) {
        Ok(units) => units,
        // With no requested units no provider call occurred, so an ordinary Full
        // request remains safe. After a real incremental request every failure is
        // result-unknown and must return to the sync recovery boundary.
        Err(_error) if !incremental_sent => return Ok(None),
        Err(error) => return Err(error.task_failure()),
    };
    let run_id = format!("run_{}", new_ulid(repo.canonical_root()));
    let failed_kinds = failed_unit_retry_kinds(&response);
    let manifest = manifest_from_units_with_failed_kinds(
        prepared_units,
        &units,
        &task.input_hash,
        &profile_tool_hash,
        0,
        Some(previous.manifest.r#gen),
        &run_id,
        &generated_at,
        &failed_kinds,
        RetryErrorKind::NetworkError,
    );
    if let Err(_error) =
        persist_normalized_instance_with_control(repo.kio_dir(), &manifest, &units, repo.control())
    {
        if !incremental_sent {
            return Ok(None);
        }
        return Err(TaskExecutionFailure {
            retry_kind: persist_failure_retry_kind(),
            retry_after_ms: None,
        });
    }
    let status = task_status_from_unit_counts(
        units.len(),
        prepared_units.len().saturating_sub(units.len()),
        false,
    );
    Ok(Some(OnlineExecutionOutcome {
        output_ref: normalized_output_ref(repo, &task.input_hash, &profile_tool_hash, 0),
        status,
        mode: MarkdownizeMode::Incremental,
        previous_raw_hash: Some(previous.manifest.raw_hash.clone()),
        parent_run_id: Some(previous.manifest.run_id.clone()),
        changed_unit_keys: incremental_hints.changed_unit_keys.clone(),
        // QA17: the real send's self-reported usage (`None` for the 0-change
        // branch above, which never called the adapter).
        usage: response.usage.clone(),
    }))
}

/// R13-1: the latest prior online-markdownize instance for `input_path` (the DONE
/// online task carries `fallback_reason = "online_adapter_done"` and an output_ref
/// pointing at its normalized instance). Distinct from `previous_instance_for_path`
/// (which the offline route uses, filtering out online refs) — here we WANT the
/// online instance and its own resolved `tool_profile_hash`.
fn latest_online_instance_for_path(
    task_store: &TaskStore,
    input_path: &str,
) -> Result<Option<PreviousInstance>> {
    let mut tasks = task_store
        .all()
        .map_err(pipeline_to_kio)?
        .into_iter()
        .filter(|task| {
            task.input_path == input_path
                && matches!(task.status, TaskStatus::Done | TaskStatus::Partial)
                && task.fallback_reason.as_deref() == Some("online_adapter_done")
        })
        .collect::<Vec<_>>();
    tasks.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    for task in tasks {
        if let Some(previous) = load_previous_instance_for_task(task_store, &task)? {
            return Ok(Some(previous));
        }
    }
    Ok(None)
}

/// R13-1: consecutive prior online incremental runs for `input_path` (docs/04
/// §3.1 condition 5 — force Full after N to bound style drift). Counts back from
/// the most recent DONE online task while it was `mode = Incremental`.
fn consecutive_online_incremental_count(task_store: &TaskStore, input_path: &str) -> Result<u32> {
    let mut tasks = task_store
        .all()
        .map_err(pipeline_to_kio)?
        .into_iter()
        .filter(|task| {
            task.input_path == input_path
                && task.status == TaskStatus::Done
                && task.fallback_reason.as_deref() == Some("online_adapter_done")
        })
        .collect::<Vec<_>>();
    tasks.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    let mut count = 0u32;
    for task in tasks {
        if task.mode == Some(MarkdownizeMode::Incremental) {
            count = count.saturating_add(1);
        } else {
            break;
        }
    }
    Ok(count)
}

/// R10-5: a `persist_normalized_instance` failure occurs AFTER a successful (and
/// already-billed, F8) OCR call, so it is a write-side I/O fault (ENOSPC / EIO /
/// interrupted fsync / a transient permission glitch), never bad input. Classify it
/// as a retryable `NetworkError` (retryable I/O) so `batch retry` can re-drive it
/// once the disk condition clears, instead of a non-retryable `InvalidInput` that
/// strands the billed, normalized output forever (retry & re-index both refuse it).
fn persist_failure_retry_kind() -> RetryErrorKind {
    RetryErrorKind::NetworkError
}

fn task_failure_from_adapter(error: kio_adapter::AdapterError) -> TaskExecutionFailure {
    // QA3: capture BEFORE `match error` moves it — `Some` only for a
    // `RateLimit` whose provider response carried a parseable `Retry-After`
    // header (kio-adapter's `AdapterError::retry_after_ms`).
    let retry_after_ms = error.retry_after_ms();
    let retry_kind = match error {
        kio_adapter::AdapterError::Auth(_) => RetryErrorKind::AuthError,
        kio_adapter::AdapterError::LocalPeerAuth(_) => RetryErrorKind::LocalPeerAuth,
        kio_adapter::AdapterError::LocalPeerConfig { .. } => RetryErrorKind::LocalPeerConfig,
        kio_adapter::AdapterError::RateLimit { .. } => RetryErrorKind::RateLimit,
        kio_adapter::AdapterError::QuotaExceeded(_) => RetryErrorKind::QuotaExceeded,
        kio_adapter::AdapterError::Network(_) | kio_adapter::AdapterError::Io { .. } => {
            RetryErrorKind::NetworkError
        }
        kio_adapter::AdapterError::ContractViolation(_)
        | kio_adapter::AdapterError::ConfigSchema(_)
        | kio_adapter::AdapterError::ConfigSchemaCoded { .. } => RetryErrorKind::ContractViolation,
    };
    TaskExecutionFailure {
        retry_kind,
        retry_after_ms,
    }
}

// ===========================================================================
// K4 — embedding catalog wiring (adapter selection, index generation, query
// embedding).
// ===========================================================================

const EMBEDDING_ADAPTER_KIND: &str = "embedding";
/// Chunks planned, reserved, and sent per pass of the sync send loop.
///
/// NOT the adapter-call granularity: `send_embed_group` makes one call per
/// group, so that a row settles on a token count the provider reported for
/// that row. This only bounds how much work one plan/reserve cycle takes on.
const EMBEDDING_BATCH_SIZE: usize = 32;

fn embedding_execution(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    control: &ControlHandle,
) -> Option<EmbeddingExecution> {
    active_embedding_execution_with_control(adapter_settings, control)
}

fn declared_embedding_profile(execution: EmbeddingExecution) -> DeclaredEmbeddingProfile {
    declared_embedding_profile_for(execution)
}

/// Whether the active embedding execution is the remote, billable API lane
/// (07 §3).
///
/// An `OfflineApi` may still be an authenticated local HTTPS peer, so this is
/// deliberately not a transport-safety predicate. That predicate is
/// [`embedding_transport_allowed`]. The cost ledger and batch lane each need
/// this remote-versus-local distinction and nothing finer.
fn embedding_is_online(execution: EmbeddingExecution) -> bool {
    execution.is_online()
}

/// The evaluator runs in this process and never opens a transport. It is the
/// only embedding execution exempt from network grants and `--offline`.
fn embedding_is_deterministic(execution: EmbeddingExecution) -> bool {
    embedding_mode_is_in_process(execution.execution_mode())
}

fn embedding_mode_is_in_process(mode: ExecutionMode) -> bool {
    mode == ExecutionMode::DeterministicLibrary
}

fn embedding_mode_transport_allowed(mode: ExecutionMode, offline: bool) -> bool {
    embedding_mode_is_in_process(mode) || !offline
}

fn embedding_transport_allowed(execution: EmbeddingExecution, offline: bool) -> bool {
    embedding_mode_transport_allowed(execution.execution_mode(), offline)
}

/// Run the active embedding adapter over a batch of items, returning one vector
/// per item.
fn run_embedding_adapter(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    execution: EmbeddingExecution,
    items: Vec<EmbeddingItem>,
    input_type: EmbeddingInputType,
    // QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880): the caller's
    // ledger `intent_token` for this send — see
    // `kio_adapter::types::EmbeddingRequest::idempotency_token`'s doc.
    idempotency_token: Option<String>,
    control: &ControlHandle,
) -> std::result::Result<kio_adapter::catalog::AdoptedEmbeddingOutcome, TaskExecutionFailure> {
    run_embedding_with_control(
        std::sync::Arc::clone(adapter_settings),
        execution,
        items,
        input_type,
        idempotency_token,
        control,
    )
    .map_err(task_failure_from_adapter)
}

/// The `fallback_reason`s [`compute_query_embedding_page1`] can report through
/// `VectorAvailability::Unavailable` beyond the pre-existing
/// `query_embedding_unavailable` (05 §1.7).
enum QueryEmbeddingOutcome {
    Vector(Vec<f32>),
    /// CL54: a live (non-stale) in-flight claim from another process already
    /// holds this exact query's device row — never send, never overwrite its
    /// token.
    InFlight,
    /// PC7 (05 §1.1 L32 / 07-adapter-spec.md §5.3): the adapter's response
    /// failed its acceptance check (e.g. a dimension mismatch) — a distinct
    /// technical failure from a plain adapter error.
    ContractViolation,
    /// PC6 (05 §1.1 L50-53 / 07 §3 L144-146): the final pre-attempt re-read
    /// found the send consent gate no longer satisfied (a revoke completed
    /// between the caller's precheck and this re-read) — refuse the send.
    /// Maps to
    /// `VectorAvailability::Unauthorized`, not `Unavailable`, at the caller.
    NotAuthorized,
}

/// vector|hybrid search page 1's query embedding call, wired onto the sync
/// degenerate 2-phase device row (04 §5.4 §H, CL48-55 in
/// tasks/step4b-contract-tests-ledger.md): a bounded sweep of stale/prunable
/// device rows first (CL52 — own key unconditionally, pruning >= 128 of the
/// shared 256-row cap), then phase 1 claim (`device_claim`: CL54 in-flight
/// text-fallback, or a fresh/reclaimed-and-fresh `stale_after_at` reservation),
/// the response's provider request id recorded durably BEFORE the terminal Tx
/// (CL43 — this Adapter integration reports no real id, so the intent_token is
/// used, the DDL's own documented fallback), and a terminal settlement
/// (success billed at the reservation estimate, or `unknown_settled` on
/// adapter failure — CL45/CL69's "no post-hoc query capability" posture, same
/// as the task-charge helpers above). R23-02: device rows are NOT exempt from
/// the device/per_adapter(embedding) cap — `ops::device_claim` now performs
/// that judgement itself, inside the same claim Tx, and returns `Denied` on
/// exceedance (04 §5.4: "device cap / per_adapter (embedding) の合算には通常
/// どおり含まれる — 判定式は不変"). Only folder cap does not apply, matching
/// the same clause's "folder cap 判定 ... には現れず" — this claim has no
/// single owning `.kio` folder to read one from (`exec_scopes` may span zero,
/// one, or many), so only the device-layer config (`~/.config/kio/config.toml`)
/// is consulted, never a folder's `.kio/config.toml`.
///
/// Only called for a FRESH page 1 (no cursor) — a cursor replay (page 2+) calls
/// the unmetered [`compute_query_embedding`] unchanged, exactly as before this
/// session's ledger migration (item 2 of the implementation instructions scopes
/// the device-row protocol to "page 1" specifically; 04 §5.4 §H's own text is
/// page-1-scoped: "vector|hybrid 検索 page 1 の query embedding 呼出").
///
/// The caller supplied the policy-filtered scopes and their retained repository
/// handles. This function selects one authorized scope, holds its writer lease,
/// and performs the final exact device+scope grant check before the adapter call.
fn compute_query_embedding_page1(
    invocation: &Invocation<'_>,
    query: &str,
    exec_scopes: &[ExecScope],
    search_repositories: &BTreeMap<String, Repository>,
    tool_id: &str,
    offline: bool,
    current_policies: &BTreeMap<String, CurrentPolicyEvaluator>,
) -> Result<Option<QueryEmbeddingOutcome>> {
    let adapter_settings = invocation.settings;

    if query.chars().count() < 2 {
        return Ok(None);
    }
    let Some(execution) = embedding_execution(adapter_settings, invocation.control) else {
        return Ok(None);
    };
    if embedding_is_deterministic(execution) {
        revalidate_current_policy_map(current_policies, false)?;
        return local_query_embedding(adapter_settings, execution, query, invocation.control);
    }
    if !embedding_transport_allowed(execution, offline) {
        return Ok(None);
    }
    let profile = declared_embedding_profile(execution);

    // PC6: the final authorization check is deliberately before *any* writable
    // source ledger action. A revoked auto/hybrid candidate therefore resolves to its
    // normal text fallback without opening the ledger, refreshing write-seq,
    // sweeping stale rows, or creating a claim. Once the check succeeds, later
    // claim/reservation/send/settlement outcomes remain the normal fresh-page-1
    // metered path.
    // Select one currently-authorized scope deterministically, then acquire only
    // that scope's retained writer lease. Rechecking the exact device+scope grant
    // while the lease is held makes a concurrent revoke either win before this
    // request or wait until this in-flight request completes.
    let mut candidates = exec_scopes
        .iter()
        .filter_map(|exec| {
            search_repositories
                .get(&exec.target.scope_id)
                .map(|repo| (exec.target.scope_id.as_str(), repo))
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|(scope_id, _)| *scope_id);
    let mut authorizing_repo = None;
    for (_, repo) in candidates {
        if persistent_network_allowed_for(
            adapter_settings,
            repo,
            approvals::AdapterRole::Embedding,
            tool_id,
            &profile.profile_hash,
        )? {
            authorizing_repo = Some(repo);
            break;
        }
    }
    let Some(authorizing_repo) = authorizing_repo else {
        return Ok(Some(QueryEmbeddingOutcome::NotAuthorized));
    };
    maybe_wait_at_test_search_final_consent_barrier(invocation.control);
    let _authorizing_scope_lock =
        authorizing_repo.lock_store_in_session(authorizing_repo.lock_session())?;
    revalidate_current_policy_map(current_policies, false)?;
    if !persistent_network_allowed_for(
        adapter_settings,
        authorizing_repo,
        approvals::AdapterRole::Embedding,
        tool_id,
        &profile.profile_hash,
    )? {
        return Ok(Some(QueryEmbeddingOutcome::NotAuthorized));
    }

    if !embedding_is_online(execution) {
        return local_query_embedding(adapter_settings, execution, query, invocation.control);
    }

    let ledger = open_ledger_db(invocation.control)?;
    let conn = &ledger;
    let key = LedgerTaskKey::new(
        LedgerTaskKey::DEVICE_SCOPE_ID,
        EMBEDDING_ADAPTER_KIND,
        device_input_hash(query),
        profile.profile_hash.clone(),
    );

    // CL52: bounded sweep BEFORE the claim — own key's stale row (if any) is
    // swept unconditionally (outside the 256 cap), pruning gets >= 128 of the
    // shared cap, decided by `allocate_sweep_capacity`'s fixed, deterministic
    // rule. Never queries the provider (CL53 — search stays responsive).
    let now = kio_pipeline::ledger::time::now_millis();

    // §M note-2: the "effective timeout" is max(participating scopes' resolved
    // `timeout_seconds`) — currently a no-op maximum, since
    // `adapter.policy.timeout_seconds` other than the documented default (300)
    // is loud-rejected at config load (`kio_core::scope`'s R12-2 decision: real
    // per-adapter timeout wiring is a separate, larger change no scope's config
    // can currently deviate from), so every participating scope's resolved
    // value is always exactly 300 and the max is trivially 300 too.
    //
    // R23-02: resolve the device-layer cap config (no folder cap — this claim
    // has no single owning `.kio` folder). `read_budget_policy` requires a
    // second (folder) path argument that this call has no legitimate value
    // for; a deliberately-nonexistent sentinel path reads back as an empty
    // `ParsedBudgetConfig::default()` (`NotFound` → defaults, `budget.rs`'s own
    // documented behavior), and `budget_cap_config`'s resulting `folder_cap` is
    // never read below — only `device_cap`/`device_per_adapter_cap` are used.
    let device_config_path = user_config_toml_path(invocation.control);
    let no_folder_cap_sentinel =
        device_config_path.with_file_name("__kio_device_claim_no_folder_cap__.toml");
    let budget_caps = read_budget_policy(&device_config_path, &no_folder_cap_sentinel)
        .map_err(pipeline_to_kio)?;
    let device_cap_config = budget_cap_config(
        &budget_caps,
        LedgerTaskKey::DEVICE_SCOPE_ID,
        EMBEDDING_ADAPTER_KIND,
    );
    let claim = kio_pipeline::ledger::ops::sweep_then_device_claim(
        conn,
        &key,
        now,
        estimate_embedding_cost(adapter_settings, query, query_embedding_send_lane()),
        TASK_SYNC_EFFECTIVE_TIMEOUT_SECONDS,
        device_cap_config.device_cap,
        device_cap_config.device_per_adapter_cap,
    )
    .map_err(pipeline_to_kio)?;
    let intent_token = match claim {
        ClaimOutcome::InFlight => return Ok(Some(QueryEmbeddingOutcome::InFlight)),
        // R23-02: cap exceeded — no reservation was made; fall back to text
        // search via the existing bare-`None` branch at the search command's
        // own call site (`vector_unavailable_reason` stays at its default
        // `"query_embedding_unavailable"`). A dedicated `"budget_denied"`
        // reason would need a new `QueryEmbeddingOutcome` variant, which would
        // widen the exhaustive match at that call site (main.rs ~L1771) —
        // that call site is outside this item's assigned main.rs range and is
        // owned by a parallel agent for this fix wave, so it is left
        // unmodified and this is flagged in the report.
        ClaimOutcome::Denied(_layer) => return Ok(None),
        ClaimOutcome::Claimed(outcome) => outcome.intent_token,
    };

    // O2 regression seam: mark that the query is about to be SENT to the embedding
    // endpoint, so a test can prove `--text` never reaches this path. No-op unless
    // the env var is set (mirrors the KIO_TEST_* adapter seams).
    record_query_embed_trace(query, invocation.control);
    let items = vec![EmbeddingItem::text("query", query)];
    // QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880): a single-item
    // sync query embed with its own single ledger reservation. Both sync sites
    // are now 1 token to 1 HTTP call — `send_embed_group` reaches it by
    // sending one group per call rather than by being inherently single.
    match run_embedding_adapter(
        adapter_settings,
        execution,
        items,
        EmbeddingInputType::Query,
        Some(intent_token.clone()),
        invocation.control,
    ) {
        Ok(outcome) => {
            // This adapter returns usage and vectors but no provider request
            // identifier or result-lookup capability. Keep batch_job_id absent;
            // a local intent token is not a queryable provider request ID.
            // I12: `:batchEmbedContents` reports `usageMetadata.promptTokenCount`
            // as a per-CALL total, which is the granularity it bills at, so this
            // settles on the measured number. The reservation estimate remains
            // the fallback for a response that carried no usable report.
            let billed = embedding_billed_from_usage(
                adapter_settings,
                outcome.usage.as_ref(),
                query_embedding_send_lane(),
                estimate_embedding_cost(adapter_settings, query, query_embedding_send_lane()),
                invocation.control,
                invocation.locks,
            );
            settle_task_charge_success(&ledger, &key, &intent_token, billed)?;
            Ok(outcome
                .vectors
                .into_iter()
                .next()
                .map(|vector| QueryEmbeddingOutcome::Vector(vector.vector)))
        }
        Err(failure) => {
            let billed_usd =
                estimate_embedding_cost(adapter_settings, query, query_embedding_send_lane());
            settle_task_charge_unknown(&ledger, &key, &intent_token, billed_usd)?;
            // PC7: classify a contract-violation adapter failure distinctly
            // from a generic technical unavailability.
            if failure.retry_kind == RetryErrorKind::ContractViolation {
                Ok(Some(QueryEmbeddingOutcome::ContractViolation))
            } else {
                Ok(None)
            }
        }
    }
}

/// The non-remote counterpart of [`compute_query_embedding_page1`]: embed the
/// query and return the vector with no ledger row. The caller has already
/// admitted an `OfflineApi` under an exact grant and writer lease; the
/// deterministic evaluator needs neither because it is in-process.
///
/// The failure classification is deliberately identical to the online path's —
/// a contract violation is still a contract violation when it comes from a
/// local server, and 05 §1.1 keeps `embedding_contract_violation` subject to
/// `fail_behavior` regardless of where the vector came from.
fn local_query_embedding(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    execution: EmbeddingExecution,
    query: &str,
    control: &ControlHandle,
) -> Result<Option<QueryEmbeddingOutcome>> {
    // O2 seam: the query is about to be embedded. That `--text` must never
    // reach this point holds for a local adapter exactly as for a remote one.
    record_query_embed_trace(query, control);
    let items = vec![EmbeddingItem::text("query", query)];
    // No `idempotency_token`: that identifies a ledger reservation, and this
    // path makes none.
    match run_embedding_adapter(
        adapter_settings,
        execution,
        items,
        EmbeddingInputType::Query,
        None,
        control,
    ) {
        Ok(outcome) => Ok(outcome
            .vectors
            .into_iter()
            .next()
            .map(|vector| QueryEmbeddingOutcome::Vector(vector.vector))),
        Err(failure) if failure.retry_kind == RetryErrorKind::ContractViolation => {
            Ok(Some(QueryEmbeddingOutcome::ContractViolation))
        }
        Err(_) => Ok(None),
    }
}

/// Append the query to the file named by `KIO_TEST_QUERY_EMBED_TRACE`, if set, at
/// the point the query embedding is sent (O2 test seam only; no-op in production).
#[cfg(debug_assertions)]
fn record_query_embed_trace(query: &str, control: &ControlHandle) {
    let trace_path = control.debug().cli.query_embed_trace.clone();
    if let Some(path) = trace_path {
        let mut line = query.to_owned();
        line.push('\n');
        let _ = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut file| file.write_all(line.as_bytes()));
    }
}

#[cfg(not(debug_assertions))]
fn record_query_embed_trace(_query: &str, _control: &ControlHandle) {}

/// The tool-lock `embedding` entry (07 §6) for the active seam, or `None` when no
/// embedding adapter is configured.
fn embedding_tool_lock_entry(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    control: &ControlHandle,
) -> Result<Option<Value>> {
    let Some(execution) = embedding_execution(adapter_settings, control) else {
        return Ok(None);
    };
    let profile = declared_embedding_profile(execution);
    // `kind`/`mode` used to be literals, which recorded an offline adapter as
    // having run online. They are properties of the adapter, so they come from
    // its profile — the lock is provenance, and provenance that always says
    // "online" is not provenance (07 §6).
    let execution_mode =
        embedding_adapter_profile_with_control(adapter_settings, execution, control)
            .map_err(adapter_to_kio)?
            .execution_mode;
    let (kind, mode) = embedding_execution_labels(execution_mode);
    Ok(Some(json!({
        "tool_id": profile.tool_id,
        "profile_hash": profile.profile_hash,
        "dimensions": profile.dimensions,
        "distance": profile.distance,
        "modality": profile.modality,
        "kind": kind,
        "mode": mode,
    })))
}

fn embedding_execution_labels(mode: ExecutionMode) -> (&'static str, &'static str) {
    match mode {
        ExecutionMode::OnlineApi => ("online_api", "online"),
        ExecutionMode::OfflineApi => ("offline_api", "offline"),
        ExecutionMode::DeterministicLibrary => ("deterministic_library", "deterministic"),
    }
}

/// Context-qualified unit of task lifecycle work. Raw retained candidates are
/// bound only after the current embedding profile is known.
#[derive(Clone)]
struct EmbeddableChunk {
    retained: RetainedEmbeddingChunk,
    work_key: EmbeddingWorkKey,
}

impl std::ops::Deref for EmbeddableChunk {
    type Target = RetainedEmbeddingChunk;
    fn deref(&self) -> &Self::Target {
        &self.retained
    }
}

impl AsRef<RetainedEmbeddingChunk> for EmbeddableChunk {
    fn as_ref(&self) -> &RetainedEmbeddingChunk {
        &self.retained
    }
}
impl AsRef<RetainedEmbeddingChunk> for RetainedEmbeddingChunk {
    fn as_ref(&self) -> &RetainedEmbeddingChunk {
        self
    }
}

fn bind_embedding_work(
    candidates: Vec<RetainedEmbeddingChunk>,
    profile: &DeclaredEmbeddingProfile,
) -> Result<Vec<EmbeddableChunk>> {
    let mut work = BTreeMap::new();
    for retained in candidates {
        let hash = chunk_embedding_hash(&retained, profile)?;
        let work_key = EmbeddingWorkKey::new(&retained.chunk_id, &hash)
            .ok_or_else(|| KioError::schema("invalid contextual embedding work identity"))?;
        work.entry(work_key.clone())
            .or_insert(EmbeddableChunk { retained, work_key });
    }
    Ok(work.into_values().collect())
}

fn retained_history_chunks(
    conn: &Connection,
    repo: &Repository,
    retained_instances: &[RetainedNormalizedInstance],
    chunking_config_hash: &str,
    current_policy: Option<&CurrentPolicyEvaluator>,
) -> Result<Vec<RetainedEmbeddingChunk>> {
    embedding_owners::collect_retained_embedding_chunks(
        repo,
        conn,
        retained_instances,
        Some(chunking_config_hash),
        current_policy,
    )
}

/// HEAD aliases win over retained history; path order breaks ties. The
/// collector has already authenticated every candidate against its exact
/// pinned normalized unit. Creation provenance is never modified.
fn selected_embedding_contexts(
    candidates: &[RetainedEmbeddingChunk],
    policy: &CurrentPolicyEvaluator,
) -> Result<BTreeMap<String, Option<String>>> {
    policy.revalidate().map_err(pipeline_to_kio)?;
    let mut selected = BTreeMap::<String, &RetainedEmbeddingChunk>::new();
    for candidate in candidates {
        if !policy
            .allows_path(&candidate.raw_path)
            .map_err(pipeline_to_kio)?
        {
            continue;
        }
        selected
            .entry(candidate.chunk_id.clone())
            .and_modify(|old| {
                if (!candidate.is_head_owner, candidate.raw_path.as_bytes())
                    < (!old.is_head_owner, old.raw_path.as_bytes())
                {
                    *old = candidate;
                }
            })
            .or_insert(candidate);
    }
    Ok(selected
        .into_iter()
        .map(|(id, candidate)| {
            (
                id,
                embedding_store::chunk_embedding_context(&candidate.raw_path),
            )
        })
        .collect())
}

/// Generate chunk embeddings for the scope after the SQLite index is rebuilt
/// (04 §4.3 / 07 §5.3). Enqueues one `TaskType::Embedding` task per pending
/// `(chunk_id, embedding_hash)` contextual work item,
/// then — if the online opt-in and budget allow — embeds them (batched), writing
/// the `embeddings` rows (source of truth) and `chunk_vec` (derived KNN copy),
/// charging the cost ledger under `adapter_kind="embedding"`. Offline leaves tasks
/// Pending (surfaced by `index_status`). No-op when no embedding adapter is
/// configured (keeps the default index path unchanged).
fn generate_scope_embeddings(
    ctx: &Invocation<'_>,
    repo: &Repository,
    args: &IndexArgs,
) -> Result<ExecOutcome> {
    let adapter_settings = ctx.settings;

    // Embedding online opt-in is the embedding adapter's own (L4), not a
    // ride-along on the markdownize approval. `--offline` forces enqueue-only;
    // N7: `--online` now reaches the embedding adapter too (was ignored).
    let online =
        embedding_online_allowed(adapter_settings, repo, args.offline, args.online, args.yes)?;
    // R11-2: return the enrichment outcome (was discarded) so `run_index` can disclose
    // it and raise the exit code on auth/budget-pause.
    run_embedding_enrichment(ctx, repo, online, false, false)
}

/// Core enrichment pass shared by `kio index` (inline) and `kio batch
/// resume/retry`. Without the resume path, embedding tasks left Pending by a
/// rate limit could never complete (`batch resume` only executed Markdownize
/// tasks). Returns the chunks embedded (executed) and failed this pass (R9-7).
fn run_embedding_enrichment(
    ctx: &Invocation<'_>,
    repo: &Repository,
    online: bool,
    override_budget: bool,
    allow_auth_revive: bool,
) -> Result<ExecOutcome> {
    let adapter_settings = ctx.settings;

    run_embedding_enrichment_for_instances(
        Invocation {
            settings: adapter_settings,
            context: ctx.context,
            control: ctx.control,
            locks: ctx.locks,
        },
        repo,
        online,
        override_budget,
        allow_auth_revive,
        None,
        None,
    )
}

/// Historical reindex drives only the selected snapshot's embedding work. In
/// particular it must not reconcile/retire task rows belonging to non-selected
/// history merely because they are absent from this deliberately narrow set.
fn run_historical_embedding_enrichment(
    ctx: &Invocation<'_>,
    repo: &Repository,
    online: bool,
    selected_instances: &[RetainedNormalizedInstance],
    selected_tree_config_hash: &str,
) -> Result<ExecOutcome> {
    let adapter_settings = ctx.settings;

    run_embedding_enrichment_for_instances(
        Invocation {
            settings: adapter_settings,
            context: ctx.context,
            control: ctx.control,
            locks: ctx.locks,
        },
        repo,
        online,
        false,
        false,
        Some(selected_instances),
        Some(selected_tree_config_hash),
    )
}

fn run_embedding_enrichment_for_instances(
    invocation: Invocation<'_>,
    repo: &Repository,
    online: bool,
    override_budget: bool,
    allow_auth_revive: bool,
    selected_instances: Option<&[RetainedNormalizedInstance]>,
    selected_tree_config_hash: Option<&str>,
) -> Result<ExecOutcome> {
    let ctx = &invocation;
    let adapter_settings = invocation.settings;

    let reconcile_scope_tasks = selected_instances.is_none();
    let Some(execution) = embedding_execution(adapter_settings, invocation.control) else {
        return Ok(ExecOutcome::default());
    };
    let profile = declared_embedding_profile(execution);
    // Non-multimodal is rejected at materialize_tool_lock; never reach embed here.
    if profile.modality != "multimodal" {
        return Ok(ExecOutcome::default());
    }
    // Local computation and enqueue-only passes do not own billing mutations.
    // Reconciliation below still completes unbilled materialized tasks; stamped
    // reservations stay retained until an online recovery has ledger authority.
    let ledger = if online && embedding_is_online(execution) {
        Some(open_ledger_db(invocation.control)?)
    } else {
        None
    };
    let db_path = sqlite_path(repo.kio_dir());
    if !db_path.exists() {
        return Ok(ExecOutcome::default());
    }
    let conn = open_existing_source_index_connection(
        &db_path,
        ExistingSourceIndexOpenMode::ReadWrite,
        &FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        },
    )
    .map_err(index_to_kio)?;
    let Some(head) = repo.head_commit_hash()? else {
        return Ok(ExecOutcome::default());
    };
    // Ordinary enrichment selects only the association bound by immutable
    // HEAD. A historical caller supplies its explicitly selected tree's hash;
    // neither path may consult mutable config.toml.
    let chunking_config_hash = match selected_tree_config_hash {
        Some(hash) => hash.to_owned(),
        None => snapshot_chunking_config_hash(repo, &head)?,
    };
    let retained_instances_owned;
    let retained_instances = if let Some(selected) = selected_instances {
        selected
    } else {
        // A bare `kio snapshot` can advance HEAD without projecting source
        // `tree_entries`. Enrichment is writer work, so materialize that source
        // cache relation here before its live-chunk JOIN.
        ensure_snapshot_tree_entries(repo, &conn, &head)?;
        retained_instances_owned = retained_history_instances(repo, &head)?;
        &retained_instances_owned
    };
    let current_policy = current_embedding_policy(repo)?;
    let retained_chunks = retained_history_chunks(
        &conn,
        repo,
        retained_instances,
        &chunking_config_hash,
        Some(&current_policy),
    )?;
    let projection_instances_owned;
    let projection_instances = if selected_instances.is_some() {
        projection_instances_owned = retained_history_instances(repo, &head)?;
        &projection_instances_owned
    } else {
        retained_instances
    };
    let projection_chunks = embedding_owners::collect_retained_embedding_chunks(
        repo,
        &conn,
        projection_instances,
        None,
        Some(&current_policy),
    )?;
    let selected_contexts = selected_embedding_contexts(&projection_chunks, &current_policy)?;
    let projection_chunks = bind_embedding_work(projection_chunks, &profile)?;
    let retained_chunks = bind_embedding_work(retained_chunks, &profile)?;
    let active_work_refs = retained_chunks
        .iter()
        .map(|chunk| chunk.work_key.output_ref())
        .collect::<BTreeSet<_>>();
    // The online sync ledger identifies a provider call by the exact
    // contextualized embedding input, not the task's bare text hash. Keep the
    // same identity available to crash reconciliation, where an embedded
    // chunk is intentionally absent from `pending`.
    let active_embedding_hashes_by_work_ref = retained_chunks
        .iter()
        .map(|chunk| {
            (
                chunk.work_key.output_ref(),
                chunk.work_key.embedding_hash().to_owned(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let task_store = TaskStore::new_with_control(repo.kio_dir(), repo.control().clone());
    validate_embedding_task_bindings(&task_store, &retained_chunks)?;
    let secrets_approved = embedding_secrets_allowed(adapter_settings, repo, &profile)?;
    let mut pending = retained_chunks_without_embedding(&conn, retained_chunks.clone(), &profile)?;
    let existing_tasks = task_store.all().map_err(pipeline_to_kio)?;
    // Fresh bookkeeping must never revive, complete, or restamp an existing
    // task (including retired records); those retain their recovery path.
    let all_task_refs = existing_tasks
        .iter()
        .map(|task| task.output_ref.as_str())
        .collect::<BTreeSet<_>>();
    let existing_task_refs = existing_tasks
        .iter()
        .filter(|task| {
            task.task_type == TaskType::Embedding
                && task.fallback_reason.as_deref() != Some(RETIRED_NON_LIVE)
        })
        .map(|task| task.output_ref.as_str())
        .collect::<BTreeSet<_>>();
    let mut pending_keys = pending
        .iter()
        .map(|chunk| chunk.work_key.clone())
        .collect::<BTreeSet<_>>();
    let mut fresh_cached = Vec::new();
    for chunk in &retained_chunks {
        let output_ref = chunk.work_key.output_ref();
        let secret_hold = secret_embedding_hold_required(
            execution.execution_mode(),
            secrets_approved,
            chunk.requires_secret_approval,
        );
        if !pending_keys.contains(&chunk.work_key)
            && !all_task_refs.contains(output_ref.as_str())
            && !secret_hold
        {
            // The exact contextual vector/association is already materialized.
            // A new chunk identity needs a Done record, not another execution.
            fresh_cached.push(chunk.clone());
        } else if (!existing_task_refs.contains(output_ref.as_str()) || secret_hold)
            && pending_keys.insert(chunk.work_key.clone())
        {
            // Cache presence never authorizes a currently secret-tainted alias.
            pending.push(chunk.clone());
        }
    }
    let now = invocation.control.now_utc_seconds();
    if !fresh_cached.is_empty() {
        current_policy.revalidate().map_err(pipeline_to_kio)?;
        enqueue_embedding_tasks(&task_store, repo, &fresh_cached, online, &now)?;
        let mut transitions = BTreeMap::new();
        record_embedding_transitions(&mut transitions, &fresh_cached, embedding_done_transition());
        // Only these genuinely fresh, selected refs are completed. In
        // particular, a historical pass cannot retire unrelated scope work.
        apply_embedding_transitions(&task_store, &transitions, &now, &BTreeMap::new())?;
    }
    // R12-3: reconcile task accounting for chunks that ARE embedded but whose task a
    // crash stranded Pending/Running (chunk_vec committed per batch, the task Done
    // write-back deferred to after the loop — R11-5). Must run BEFORE the
    // empty-`pending` early return: the r12k crash left every chunk embedded with 64
    // tasks still Pending, so `pending` is empty yet index_status reported phantom
    // pending enrichment forever. Idempotent; no adapter call, no re-charge.
    if reconcile_scope_tasks {
        reconcile_committed_embedding_tasks(
            repo,
            &task_store,
            ledger.as_ref(),
            EmbeddingReconcileContext {
                active_work_refs: &active_work_refs,
                active_embedding_hashes_by_work_ref: &active_embedding_hashes_by_work_ref,
                pending: &pending,
                now: &now,
                profile_hash: &profile.profile_hash,
                allow_auth_revive,
            },
        )?;
    }
    // Image objects are embedded on their own schedule, before the chunk
    // early-return below: a corpus can have every chunk embedded and still have
    // images outstanding — the first index after an image-capable adapter is
    // configured is exactly that case. Counted separately from `executed`,
    // which is a chunk-member count the task accounting is keyed to.
    let embedding_context = EmbeddingExecutionContext {
        repo,
        execution,
        profile: &profile,
        current_policy: &current_policy,
        projection: EmbeddingProjection {
            repo,
            policy: &current_policy,
            selected_contexts: &selected_contexts,
            invalidated: std::cell::Cell::new(false),
        },
    };
    let mut projection_held = BTreeSet::new();
    if secret_embedding_hold_required(execution.execution_mode(), secrets_approved, true) {
        projection_held.extend(held_secret_embedding_chunk_ids(
            repo.kio_dir(),
            repo.control(),
        )?);
        projection_held.extend(
            projection_chunks
                .iter()
                .filter(|chunk| chunk.requires_secret_approval)
                .map(|chunk| chunk.chunk_id.clone()),
        );
    }
    refresh_embedding_projection(
        ctx,
        repo,
        &conn,
        &projection_chunks,
        &profile,
        &embedding_context.projection,
        &projection_held,
    )?;
    run_image_embedding_enrichment(
        Invocation {
            settings: adapter_settings,
            context: ctx.context,
            control: ctx.control,
            locks: ctx.locks,
        },
        &embedding_context,
        &conn,
        retained_instances,
        &retained_chunks,
        secrets_approved,
        online,
    )?;
    prune_terminal_embedding_secret_holds(repo)?;
    if pending.is_empty() {
        return Ok(ExecOutcome::default());
    }
    // N1a: partition off chunks whose raw file is a Tier B (candidate-secret)
    // document without a `--send-secrets` approval. Their embedding tasks are held
    // (Paused `secrets_tier_b_hold`, visible in `kio status`) and never enter the
    // send pipeline, so `index --online` / `batch resume` cannot ship their text
    // to the embedding API. Approval moves them back into `sendable`.
    let (held, sendable): (Vec<EmbeddableChunk>, Vec<EmbeddableChunk>) =
        pending.into_iter().partition(|chunk| {
            // R19-1: hold ANY secret (Tier B, or a lifted Tier A) from the embedding
            // API unless `--send-secrets`. Non-lifted Tier A never produces chunks
            // (excluded at ingest), so `.is_some()` newly gates only lifted Tier A.
            secret_embedding_hold_required(
                execution.execution_mode(),
                secrets_approved,
                chunk.requires_secret_approval,
            )
        });
    hold_secret_embedding_tasks(&task_store, repo, ledger.as_ref(), &profile, &held, &now)?;
    if sendable.is_empty() {
        return Ok(ExecOutcome::default());
    }
    enqueue_embedding_tasks(&task_store, repo, &sendable, online, &now)?;
    if !online {
        // Offline: tasks stay Pending; `index_status` reports them (05 §1.7).
        return Ok(ExecOutcome::default());
    }

    // L2(ii)/L7: skip chunks whose embedding task is sticky budget-Paused (unless
    // override) or a Failed task that is not retry-eligible (unelapsed backoff or
    // non-retryable) — the same lifecycle semantics markdownize already honors.
    let embeddable = filter_embeddable_by_task_state(
        &task_store,
        sendable,
        override_budget,
        invocation.control,
    )?;
    if embeddable.is_empty() {
        return Ok(ExecOutcome::default());
    }

    let mut outcome = ExecOutcome::default();
    let budget_caps = read_budget_policy(
        user_config_toml_path(invocation.control),
        repo.kio_dir().join("config.toml"),
    )
    .map_err(pipeline_to_kio)?;
    let month = utc_month(&now);
    let scope_id = repo.scope_identity()?.scope_id;

    // R11-5: accumulate every chunk's task-store transition in memory and write it
    // back in ONE `update_matching` after the loop, instead of a full all()+
    // replace_all per 32-chunk batch. The per-batch form cost O(T) each × T/32
    // batches = O(T²/32), turning a few-thousand-chunk initial embedding into a
    // multi-minute hang.
    //
    // R17-7: the reuse-based crash safety here is precise about WHICH crash window it
    // covers. A crash AFTER `send_embed_group` COMPLETES but before this deferred
    // write-back is fully absorbed: `send_embed_group` already wrote the embeddings
    // row + chunk_vec and F8 already reserved the charge, so re-driving the chunk hits
    // the free content-addressed reuse path (§5.5: text_hash hit → no API call, no
    // re-charge), no unrecorded completion is double-billed, and the per-chunk map
    // keeps a reuse "done" from being contaminated by a sibling send "failed" (L6).
    // This does NOT extend to a crash INTERNAL to `send_embed_group`, in the narrow
    // window AFTER the API bills but BEFORE the embeddings row / chunk_vec commit:
    // there the embeddings are unwritten, so §5.5 reuse misses and the chunk re-enters
    // `to_send`, yet its stale RateLimit/Quota `fallback_reason` makes
    // `reservation_covers_resend` skip the re-charge — so the resend double-bills the
    // API while the reservation stays at 1× = a BOUNDED per-chunk under-charge. That is
    // exactly the triple-fault R16 DEFERRED (window = bill→chunk_vec commit; damage =
    // one chunk's cost), left standing on purpose: closing it would need a per-chunk
    // pre-send persisted marker, re-introducing R11-5's O(T²) full-tasks.jsonl write.
    // The asymmetry with markdownize is deliberate: markdownize persists status=Running
    // + `fallback_reason=None` BEFORE its send, so a crash makes it re-reserve; embedding
    // keeps the per-batch write-back (O(T²) avoidance) and instead absorbs the NORMAL
    // (post-completion) crash via content-addressed reuse. R11-2: the loop also tallies
    // paused / auth / failed outcomes.
    // 07 §5.3 の 2026-07-24 訂正: the Batch lane is the default (half rate). It
    // SUBMITS and returns; results are collected by `poll_batch_embedding_jobs`
    // from `kio batch resume` or any later write command. `--realtime` (and a
    // `[adapter] lane = "realtime"` config) selects the synchronous path below,
    // which completes inside this invocation at double the rate.
    // 05 §1.8 write-through. Accumulated across every batch and flushed once
    // per exit path: the replica is a separate database, so folding the write
    // into the per-group loop would cost one transaction per group (2,321 of
    // them in the dogfood fixture) to record a set the pass already knows.
    //
    // The generation is read BEFORE the first vector lands: it names the state
    // the delta is a change TO, which is the state the replica has to already
    // be in for the delta to complete it (R25-3). Nothing rotates during the
    // loop, so one read at the top is the whole answer.
    let replica_before = replica_scope_stamp(repo.kio_dir()).map(|(_, generation)| generation);
    let mut replica = kio_index::aggregator::ScopeDelta::default();
    // Provider Batch is an online transport. Local embeddings must take the
    // synchronous local path even when Batch is the invocation default.
    if embedding_is_online(execution) && effective_invocation_lane() == PreferredRequestKind::Batch
    {
        let mut batch_transitions: BTreeMap<String, EmbeddingTransition> = BTreeMap::new();
        if let Some(submitted) = submit_embedding_batch_jobs(
            ctx,
            repo,
            execution,
            &conn,
            ledger
                .as_ref()
                .expect("online batch embedding requires an initialized ledger"),
            &profile,
            &embeddable,
            &scope_id,
            &budget_caps,
            override_budget,
            &current_policy,
            &embedding_context.projection,
            &mut batch_transitions,
            &mut replica,
        )? {
            // Submitted members stay Pending on purpose (no transition): the
            // job is in flight and `index_status` must keep reporting them
            // until collect.
            let no_reservations: BTreeMap<String, (f64, String, String)> = BTreeMap::new();
            apply_embedding_transitions(&task_store, &batch_transitions, &now, &no_reservations)?;
            // Submitting sends nothing to the index, but the reuse links it
            // settled on the way past did — this lane's early return must carry
            // them too, rotation included: a content-addressed reuse hit writes
            // `chunk_vec` with no adapter call and no rebuild, and a vector
            // appearing is exactly the kind of change LC25 retires cursors for.
            current_policy.revalidate().map_err(pipeline_to_kio)?;
            publish_in_place_delta(ctx, repo.kio_dir(), replica_before.as_deref(), &replica)?;
            return Ok(submitted);
        }
        // Lane unavailable — fall through to the synchronous loop below, which
        // prices itself at the sync rate (`active_embedding_send_lane`).
    }

    let mut transitions: BTreeMap<String, EmbeddingTransition> = BTreeMap::new();
    // R18-1: per-chunk FRESH F8 reservation `(usd, ledger_month)` for the freshly-charged
    // chunks this pass, stamped in the single end-of-pass write-back so a later non-live
    // reclaim (R18-1) can cancel the exact phantom.
    let mut reserved_by_ref: BTreeMap<String, (f64, String, String)> = BTreeMap::new();

    // R16-7: embedding tasks whose PREVIOUS recorded failure was a non-billable
    // rejection (RateLimit / QuotaExceeded — refused before the backend processes or
    // bills the request). Their earlier F8 reservation already covers this resend, so
    // the per-batch charge below excludes their chars. Without this a rate-limited
    // chunk (RateLimit = unbounded retries) re-reserves its chars on every `batch
    // retry`, and one logical enrichment bills N times, exhausting the device month cap
    // and falsely pausing unrelated tasks (R15-2's harm on the retry path). Loaded once
    // at pass start (transitions are written back only after the loop), so it reflects
    // the PREVIOUS pass's outcome — exactly the "prior failure" the gate needs.
    //
    // Fresh / NetworkError / crash-stranded chunks are absent from this set and charge
    // afresh: a NetworkError send may have been billed server-side, and a crash mid-send
    // leaves the task Pending with its embedding uncommitted, so re-driving re-reserves
    // conservatively (bounded by max_attempts; a succeeded-but-uncommitted chunk instead
    // re-drives through §5.5 content-addressed reuse with NO re-charge). Reserving once
    // for the task's lifetime is deliberately NOT done — it would let real spend exceed
    // the reserved cap on server-side-billed retries (the R15-5 silent cap bypass).
    let embedding_tasks_by_ref = task_store
        .all()
        .map_err(pipeline_to_kio)?
        .into_iter()
        .filter(|task| task.task_type == TaskType::Embedding)
        .map(|task| (task.output_ref.clone(), task))
        .collect::<BTreeMap<_, _>>();

    'embedding_batches: for batch in embeddable.chunks(EMBEDDING_BATCH_SIZE) {
        // Split content-addressed reuse (no API call, free) from chunks that
        // require a live adapter call (CT3-EMBED-006).
        let plan = match plan_embed_batch(&conn, &profile, batch) {
            Ok(plan) => plan,
            Err(failure) => {
                record_embedding_transitions(
                    &mut transitions,
                    batch.iter(),
                    embedding_fail_transition(failure.retry_kind),
                );
                count_embedding_failure(&mut outcome, failure.retry_kind, batch.len());
                break;
            }
        };
        // L6: reuse links are free and always succeed → link + complete them up
        // front so an API failure on the *sent* portion can never contaminate an
        // already-materialized (chunk_vec written) chunk into a stuck Failed task.
        if !plan.reuse.is_empty() {
            match link_reused_chunks(
                &conn,
                &profile,
                &plan.reuse,
                &embedding_context.projection,
                &mut replica,
            ) {
                Ok(()) => {
                    record_embedding_transitions(
                        &mut transitions,
                        plan.reuse.iter().map(|(chunk, _)| *chunk),
                        embedding_done_transition(),
                    );
                    outcome.executed += plan.reuse.len();
                }
                Err(failure) => {
                    record_embedding_transitions(
                        &mut transitions,
                        plan.reuse.iter().map(|(chunk, _)| *chunk),
                        embedding_fail_transition(failure.retry_kind),
                    );
                    count_embedding_failure(&mut outcome, failure.retry_kind, plan.reuse.len());
                    break 'embedding_batches;
                }
            }
        }
        if plan.to_send.is_empty() {
            continue;
        }
        // Each `to_send` GROUP (one deduplicated content identity —
        // `embedding_hash`, fanned out to every member chunk sharing that exact
        // contextualized input) is ONE ledger reservation, keyed by that exact
        // `embedding_hash` — never by a representative task's bare `text_hash`:
        // the latter can collide across distinct filename contexts. A group's
        // members share ONE estimate
        // (`group.representative.text.chars().count()`). The reservation itself
        // is intentionally delayed until the group is about to make its one
        // provider call, so an earlier failure cannot strand rows for groups
        // that never left Kio.
        struct GroupCharge {
            key: LedgerTaskKey,
            candidate_usd: f64,
        }
        let mut charge_by_group: Vec<GroupCharge> = Vec::with_capacity(plan.to_send.len());
        for group in &plan.to_send {
            let output_ref = embedding_task_output_ref(group.representative);
            if !embedding_tasks_by_ref.contains_key(&output_ref) {
                return Err(KioError::schema(
                    "embedding send has no matching task reservation owner",
                ));
            }
            let key = task_ledger_key(
                &scope_id,
                EMBEDDING_ADAPTER_KIND,
                &group.embedding_hash,
                &profile.profile_hash,
            );
            // R24 (3 系統一致): price the string actually sent. The send below
            // contextualizes the chunk text, so estimating the bare text
            // undercounts by the context prefix — and `usage: None` means the
            // shortfall is never corrected at settle time (QA17).
            let candidate_usd = estimate_embedding_cost(
                adapter_settings,
                &embedding_store::contextualized_embedding_input(
                    embedding_store::chunk_embedding_context(&group.representative.raw_path)
                        .as_deref(),
                    &group.representative.text,
                ),
                active_embedding_send_lane(),
            );
            // Reservation and admission deliberately happen in the send loop
            // below, immediately before this exact group reaches the provider.
            // A later group must never reserve merely because an earlier one is
            // about to be sent.
            charge_by_group.push(GroupCharge { key, candidate_usd });
        }
        // One HTTP call per group, so this row's charge is the token count the
        // provider reported for THIS row. `:batchEmbedContents` bills and
        // reports per CALL, so any call spanning several rows can only be split
        // by guesswork; the Batch lane avoids that by making the job the task
        // (07 §5.5), and the sync lane avoids it by making the call the row.
        // The realtime lane is opt-in and never the default (2026-07-25
        // ruling), so its extra round trips buy exact per-row billing at a cost
        // no default path pays.
        for (index, charge) in charge_by_group.iter().enumerate() {
            let group = &plan.to_send[index];
            let (intent_token, reserved_usd) = if embedding_is_online(execution) {
                let ledger = ledger
                    .as_ref()
                    .expect("online embedding requires an initialized ledger");
                let existing = get_batch_request(ledger, &charge.key).map_err(pipeline_to_kio)?;
                if sync_result_unknown_fenced(existing.as_ref()) {
                    record_embedding_transitions(
                        &mut transitions,
                        group.members.iter().copied(),
                        embedding_fail_transition(RetryErrorKind::ResultUnknown),
                    );
                    count_embedding_failure(
                        &mut outcome,
                        RetryErrorKind::ResultUnknown,
                        group.members.len(),
                    );
                    continue;
                }
                if existing.as_ref().is_some_and(|row| row.state.is_inflight()) {
                    if existing
                        .as_ref()
                        .is_some_and(|row| row.request_kind != RequestKind::Sync)
                    {
                        // A batch-lane row has provider-side recovery/poll
                        // authority. A realtime fallback must not steal or
                        // resend it.
                        outcome.inflight += group.members.len();
                        continue;
                    }
                    // A prior process may have sent this group before dying.
                    // Settle its unqueryable result and fence it instead of
                    // reusing that reservation for another provider request.
                    release_task_charge_if_open(ledger, &charge.key)?;
                    record_embedding_transitions(
                        &mut transitions,
                        group.members.iter().copied(),
                        embedding_fail_transition(RetryErrorKind::ResultUnknown),
                    );
                    count_embedding_failure(
                        &mut outcome,
                        RetryErrorKind::ResultUnknown,
                        group.members.len(),
                    );
                    continue;
                }
                validate_embedding_aliases(
                    &current_policy,
                    &profile,
                    group.members.iter().copied(),
                )
                .map_err(|_| {
                    KioError::schema("embedding owner policy changed before reservation")
                })?;
                admit_embedding_transport(
                    adapter_settings,
                    repo,
                    execution,
                    &profile,
                    &current_policy,
                    group
                        .members
                        .iter()
                        .any(|chunk| chunk.requires_secret_approval),
                )?;
                let caps = budget_cap_config(&budget_caps, &scope_id, EMBEDDING_ADAPTER_KIND);
                let bypass_cap_denial = override_budget || !budget_caps.hard_stop;
                match reserve_or_reuse_task_charge(
                    ctx,
                    ledger,
                    &charge.key,
                    charge.candidate_usd,
                    &caps,
                    bypass_cap_denial,
                    RequestKind::Sync,
                )? {
                    TaskChargeOutcome::BudgetExceeded => {
                        record_embedding_transitions(
                            &mut transitions,
                            group.members.iter().copied(),
                            embedding_pause_transition(),
                        );
                        outcome.paused += group.members.len();
                        continue;
                    }
                    TaskChargeOutcome::Reserved {
                        intent_token,
                        estimated_usd,
                    }
                    | TaskChargeOutcome::Reused {
                        intent_token,
                        estimated_usd,
                    } => (Some(intent_token), estimated_usd),
                }
            } else {
                (None, 0.0)
            };
            // QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880): a provider
            // idempotency key identifies one HTTP call, and now every call is
            // one group, so each send carries its OWN reservation's token
            // rather than a representative standing in for the batch.
            match send_embed_group(
                adapter_settings,
                &conn,
                &embedding_context,
                group,
                intent_token.clone(),
                &mut replica,
                invocation.control,
            ) {
                Ok(usage) => {
                    // No reservation was made offline, so there is nothing to
                    // settle and no `reservation_id` for recovery to walk.
                    if let Some(intent_token) = intent_token.as_ref() {
                        settle_task_charge_success(
                            ledger
                                .as_ref()
                                .expect("online embedding requires an initialized ledger"),
                            &charge.key,
                            intent_token,
                            embedding_billed_from_usage(
                                adapter_settings,
                                usage.as_ref(),
                                active_embedding_send_lane(),
                                reserved_usd,
                                invocation.control,
                                invocation.locks,
                            ),
                        )?;
                        // R18-1's stamp, still needed so `reconcile_committed_embedding_tasks`
                        // et al. can find this group's ledger selector via
                        // `task.reservation_id` without reconstructing the key from
                        // possibly-since-changed config.
                        reserved_by_ref.insert(
                            embedding_task_output_ref(group.representative),
                            (reserved_usd, month.clone(), intent_token.clone()),
                        );
                    }
                    record_embedding_transitions(
                        &mut transitions,
                        group.members.iter().copied(),
                        embedding_done_transition(),
                    );
                    outcome.executed += group.members.len();
                }
                Err(failure) => {
                    let known_rejection = matches!(
                        failure.retry_kind,
                        RetryErrorKind::AuthError
                            | RetryErrorKind::RateLimit
                            | RetryErrorKind::QuotaExceeded
                    );
                    if let Some(intent_token) = intent_token.as_ref() {
                        if known_rejection {
                            terminal_transaction(
                                ledger.as_ref().expect("online embedding requires ledger"),
                                &TerminalWrite {
                                    key: &charge.key,
                                    outcome: Outcome::SubmitRejected,
                                    billed: BilledAmount {
                                        usd: 0.0,
                                        estimated: false,
                                    },
                                    ledger_batch_job_id: intent_token,
                                    next_state: BatchState::Terminal,
                                    error: Some(retry_reason(failure.retry_kind)),
                                    increment_contract_violation: false,
                                    attempts_delta: 1,
                                    clear_intent_token: true,
                                    intent_token_guard: Some(intent_token),
                                    reseat_submission_seq: false,
                                },
                            )
                            .map_err(pipeline_to_kio)?;
                        } else {
                            settle_task_charge_unknown(
                                ledger.as_ref().expect("online embedding requires ledger"),
                                &charge.key,
                                intent_token,
                                reserved_usd,
                            )?;
                        }
                    }
                    let result_unknown = embedding_is_online(execution) && !known_rejection;
                    let kind = if result_unknown {
                        RetryErrorKind::ResultUnknown
                    } else {
                        failure.retry_kind
                    };
                    record_embedding_transitions(
                        &mut transitions,
                        group.members.iter().copied(),
                        if result_unknown {
                            embedding_fail_transition(kind)
                        } else {
                            embedding_failure_transition(&failure)
                        },
                    );
                    count_embedding_failure(&mut outcome, kind, group.members.len());
                    break 'embedding_batches;
                }
            }
        }
    }
    // R11-5: single write-back for the whole pass. Its return is the retry-eligible
    // failed count (needs per-task `attempts`), feeding the batch exit-code 3-vs-4
    // split (R11-2). `fallback_reason` per chunk (done/paused/failed) is preserved.
    outcome.failed_retryable +=
        apply_embedding_transitions(&task_store, &transitions, &now, &reserved_by_ref)?;
    // 05 §1.8 write-through, after the loop's `break` paths as well as its
    // normal exit: a pass that gave up halfway still linked every vector it got
    // to, and those belong in the corpus.
    //
    // R25-4/R25-12: this lane writes `chunk_vec` straight into the live index.
    // Under `kio index` a rebuild has already rotated, but the same function is
    // reached from `kio batch resume` when the batch lane is unavailable, and
    // there nothing else rotates at all — leaving both the cursor contract and
    // the replica's only staleness signal on a stamp that never moves.
    current_policy.revalidate().map_err(pipeline_to_kio)?;
    publish_in_place_delta(ctx, repo.kio_dir(), replica_before.as_deref(), &replica)?;
    if outcome.executed > 0 && replica.is_empty() {
        write_through_projection_or_log_for_repo(ctx, repo);
    }
    Ok(outcome)
}

/// A local `offline_api` may be an authenticated HTTP peer. Secret approval is
/// therefore a transport property, rather than an online-provider property.
fn secret_embedding_hold_required(
    execution_mode: ExecutionMode,
    secrets_approved: bool,
    requires_secret_approval: bool,
) -> bool {
    execution_mode != ExecutionMode::DeterministicLibrary
        && !secrets_approved
        && requires_secret_approval
}

/// R11-5: one deferred task-store transition for an embedding chunk, applied in a
/// single pass by [`apply_embedding_transitions`].
#[derive(Clone, Copy)]
struct EmbeddingTransition {
    status: TaskStatus,
    reason: &'static str,
    failure_kind: Option<RetryErrorKind>,
    // QA3: the provider's `Retry-After` (ms) for a `"rate_limit"` transition —
    // `apply_embedding_transitions` reads this to build `next_retry_at` via
    // `rate_limit_retry_at`. `None` for every other transition.
    retry_after_ms: Option<u64>,
}

fn embedding_done_transition() -> EmbeddingTransition {
    EmbeddingTransition {
        status: TaskStatus::Done,
        reason: "embedding_adapter_done",
        failure_kind: None,
        retry_after_ms: None,
    }
}

fn embedding_pause_transition() -> EmbeddingTransition {
    EmbeddingTransition {
        status: TaskStatus::Paused,
        reason: "budget_exceeded",
        failure_kind: None,
        retry_after_ms: None,
    }
}

fn embedding_fail_transition(kind: RetryErrorKind) -> EmbeddingTransition {
    EmbeddingTransition {
        status: TaskStatus::Failed,
        reason: retry_reason(kind),
        failure_kind: Some(kind),
        retry_after_ms: None,
    }
}

/// The disposition a provider-side failure gives the affected chunks.
///
/// QA2/QA3 (04 §5.2/§5.3): `rate_limit` and `auth_error` each land a dedicated
/// non-Failed transition (mirroring the markdownize send handler's 3-way
/// branch); every other kind takes the generic Failed transition. Shared by the
/// synchronous send handler and — since G2 — the Batch submit lane, so the two
/// cannot drift apart.
fn embedding_failure_transition(failure: &TaskExecutionFailure) -> EmbeddingTransition {
    match failure.retry_kind {
        RetryErrorKind::RateLimit => embedding_rate_limit_transition(failure.retry_after_ms),
        RetryErrorKind::AuthError => embedding_auth_pause_transition(),
        _ => embedding_fail_transition(failure.retry_kind),
    }
}

/// QA3 (step4b-contract-tests-p3a.md §A, 04 §5.2/§5.3): a rate-limited
/// embedding send stays Pending (never Failed/Paused) — mirrors the
/// markdownize send handler's RateLimit branch. `apply_embedding_transitions`
/// reads `retry_after_ms` to honor the provider's `Retry-After` header (else
/// the synthetic +2s fallback), and does not bump `attempts`.
fn embedding_rate_limit_transition(retry_after_ms: Option<u64>) -> EmbeddingTransition {
    EmbeddingTransition {
        status: TaskStatus::Pending,
        reason: "rate_limit",
        failure_kind: None,
        retry_after_ms,
    }
}

/// QA2 (step4b-contract-tests-p3a.md §A, 04 §5.2): an auth-failed embedding
/// send lands Paused(hold_reason=auth) (never Failed) — mirrors the
/// markdownize send handler's AuthError branch. `apply_embedding_transitions`
/// does not bump `attempts` and clears `next_retry_at`; `hold_reason` is
/// auto-stamped by the existing QA1 `hold_reason_for_reason` block.
fn embedding_auth_pause_transition() -> EmbeddingTransition {
    EmbeddingTransition {
        status: TaskStatus::Paused,
        reason: "auth_error",
        failure_kind: None,
        retry_after_ms: None,
    }
}

/// R19-3: terminalize a non-live embedding task REVERSIBLY — non-retryable (no
/// `failure_kind`, so no backoff/attempt bump), but `retired_non_live` so the enqueue
/// guard re-creates a fresh task if the exact chunk_id reappears (revert/restore).
fn embedding_retired_non_live_transition() -> EmbeddingTransition {
    EmbeddingTransition {
        status: TaskStatus::Failed,
        reason: RETIRED_NON_LIVE,
        failure_kind: None,
        retry_after_ms: None,
    }
}

/// R11-2: tally one failed embedding batch into the pass outcome — `failed` always,
/// and `auth_failed` when the error needs user re-auth (exit 5). `failed_retryable`
/// is finalized later in [`apply_embedding_transitions`] (it needs per-task attempts).
fn count_embedding_failure(outcome: &mut ExecOutcome, kind: RetryErrorKind, count: usize) {
    outcome.failed += count;
    if kind == RetryErrorKind::AuthError {
        outcome.auth_failed += count;
    }
}

fn record_embedding_transitions<'a>(
    transitions: &mut BTreeMap<String, EmbeddingTransition>,
    chunks: impl IntoIterator<Item = &'a EmbeddableChunk>,
    transition: EmbeddingTransition,
) {
    for chunk in chunks {
        transitions.insert(embedding_task_output_ref(chunk), transition);
    }
}

/// R11-5: apply all accumulated embedding-task transitions in ONE `update_matching`.
/// Returns the count of failures recorded retry-eligible (retryable kind with
/// `attempts` remaining) so the caller can split the batch exit code 3 vs 4 (R11-2).
fn apply_embedding_transitions(
    task_store: &TaskStore,
    transitions: &BTreeMap<String, EmbeddingTransition>,
    now: &str,
    // R18-1: the FRESH per-chunk F8 reservation `(usd, ledger_month)` to stamp onto each
    // freshly-charged embedding task in this same single write-back (no extra
    // tasks.jsonl pass — respects R11-5/R17-7's O(T²) avoidance). Only freshly-charged
    // chunks are present; a RateLimit/Quota resend covered by a prior reservation is
    // absent and keeps its existing stamp. The stamp lets `reconcile_committed_embedding_tasks`
    // reclaim the exact phantom if the chunk later goes non-live (R18-1).
    reserved: &BTreeMap<String, (f64, String, String)>,
) -> Result<usize> {
    if transitions.is_empty() {
        return Ok(0);
    }
    let mut failed_retryable = 0usize;
    task_store
        .update_matching(|task| {
            if task.task_type != TaskType::Embedding {
                return false;
            }
            let Some(transition) = transitions.get(&task.output_ref) else {
                return false;
            };
            task.status = transition.status;
            task.fallback_reason = Some(transition.reason.to_owned());
            // QA1 (step4b-contract-tests-p3a.md §A): the closed hold_reason
            // enum accompanies every Paused task (currently only
            // `embedding_pause_transition`'s `budget_exceeded`).
            task.hold_reason = (transition.status == TaskStatus::Paused)
                .then(|| hold_reason_for_reason(transition.reason))
                .flatten();
            if let Some((usd, month, reservation_id)) = reserved.get(&task.output_ref) {
                task.reserved_usd = Some(*usd);
                task.reserved_month = Some(month.clone());
                task.reservation_id = Some(reservation_id.clone());
            }
            if let Some(kind) = transition.failure_kind {
                let attempts_after = task.attempts.saturating_add(1);
                let policy = retry_policy(kind);
                let retryable = policy.retryable
                    && policy
                        .max_attempts
                        .map(|max| attempts_after < max)
                        .unwrap_or(true);
                task.attempts = attempts_after;
                task.next_retry_at =
                    retryable.then(|| scheduled_retry_at(now, &policy.backoff, attempts_after));
                if retryable {
                    failed_retryable += 1;
                }
            } else if transition.reason == "rate_limit" {
                // QA3 (04 §5.3): max_attempts=∞ — `attempts` is NOT consumed.
                // `next_retry_at` honors the provider's `Retry-After` (or the
                // synthetic +2s fallback when absent).
                task.next_retry_at = Some(rate_limit_retry_at(now, transition.retry_after_ms));
                failed_retryable += 1;
            } else if transition.reason == "auth_error" {
                // QA2: a pause is not a retry-budget event — `attempts` is NOT
                // consumed. `hold_reason` is already auto-stamped above via
                // `hold_reason_for_reason`.
                task.next_retry_at = None;
            }
            true
        })
        .map_err(pipeline_to_kio)?;
    Ok(failed_retryable)
}

/// A batch split into free content-addressed reuse and chunks needing an API call.
struct EmbedBatchPlan<'a> {
    /// (chunk, existing content-vector bytes) — link `chunk_vec`, no adapter call.
    reuse: Vec<(&'a EmbeddableChunk, Vec<u8>)>,
    /// One adapter item per content identity, fanned out to every member chunk.
    to_send: Vec<EmbeddingSendGroup<'a>>,
}

/// The execution identity and current admission authority shared by chunk and
/// image embedding call edges. Keeping them together prevents either path from
/// accidentally using a profile or policy from a different execution.
struct EmbeddingExecutionContext<'a> {
    repo: &'a Repository,
    execution: EmbeddingExecution,
    profile: &'a DeclaredEmbeddingProfile,
    current_policy: &'a CurrentPolicyEvaluator,
    projection: EmbeddingProjection<'a>,
}

/// A frozen policy and the deterministic scalar choice derived from retained
/// owner bindings. Revalidation is required at every live publication edge.
struct EmbeddingProjection<'a> {
    repo: &'a Repository,
    policy: &'a CurrentPolicyEvaluator,
    selected_contexts: &'a BTreeMap<String, Option<String>>,
    invalidated: std::cell::Cell<bool>,
}

fn validate_embedding_aliases<'a>(
    policy: &CurrentPolicyEvaluator,
    profile: &DeclaredEmbeddingProfile,
    chunks: impl IntoIterator<Item = &'a EmbeddableChunk>,
) -> std::result::Result<(), TaskExecutionFailure> {
    let denied = || TaskExecutionFailure {
        retry_kind: RetryErrorKind::AuthError,
        retry_after_ms: None,
    };
    policy.revalidate().map_err(|_| denied())?;
    for chunk in chunks {
        if chunk.work_key.chunk_id() != chunk.chunk_id
            || chunk_embedding_hash(chunk, profile).map_err(|_| TaskExecutionFailure {
                retry_kind: RetryErrorKind::ContractViolation,
                retry_after_ms: None,
            })? != chunk.work_key.embedding_hash()
        {
            return Err(TaskExecutionFailure {
                retry_kind: RetryErrorKind::ContractViolation,
                retry_after_ms: None,
            });
        }
        if !policy.allows_path(&chunk.raw_path).map_err(|_| denied())? {
            return Err(denied());
        }
    }
    Ok(())
}

/// A vector writer must not mutate source scalars while an old Ready header
/// can remain observable. Unlike best-effort repair logging, failure here
/// aborts before the source mutation.
fn invalidate_embedding_projection(repo: &Repository) -> Result<()> {
    let scope_id = repo.scope_identity()?.scope_id;
    let mut replica = kio_index::aggregator::Aggregator::open_with_control(
        &aggregator_path(repo.control()),
        repo.control(),
    )
    .map_err(index_to_kio)?;
    if let Some(mut header) = replica.scope_header(&scope_id).map_err(index_to_kio)? {
        header.index_status = kio_index::aggregator::AggIndexStatus::Rebuilding;
        replica
            .update_scope_header(&scope_id, &header, replica_now_ms())
            .map_err(index_to_kio)?;
    }
    rotate_index_generation_unconditionally(repo.kio_dir(), repo.control())
}

fn invalidate_embedding_projection_once(projection: &EmbeddingProjection<'_>) -> Result<()> {
    if !projection.invalidated.get() {
        invalidate_embedding_projection(projection.repo)?;
        projection.invalidated.set(true);
    }
    Ok(())
}

/// Scalar equality alone cannot certify publication: a prior writer may have
/// committed vectors and stopped while the replica was still Rebuilding.
/// Cache probe failures take the ordinary best-effort publication path below.
fn embedding_replica_projection_is_current(repo: &Repository) -> bool {
    let Some((scope_id, generation)) = replica_scope_stamp(repo.kio_dir()) else {
        return false;
    };
    let Ok(head) = repo.head_commit_hash() else {
        return false;
    };
    let config = match head.as_deref() {
        Some(head) => match snapshot_chunking_config_hash(repo, head) {
            Ok(config) => Some(config),
            Err(_) => return false,
        },
        None => None,
    };
    let Ok(replica) = kio_index::aggregator::Aggregator::open_with_control(
        &aggregator_path(repo.control()),
        repo.control(),
    ) else {
        return false;
    };
    let Ok(Some(header)) = replica.scope_header(&scope_id) else {
        return false;
    };
    header.index_status == kio_index::aggregator::AggIndexStatus::Ready
        && header.index_generation == generation
        && header.current_snapshot_commit == head
        && header.current_chunking_config_hash == config
}

/// Refresh cached scalar choices before task completion is considered. Full
/// publication is required because an owner-policy change can remove vectors;
/// the additive fast-path delta cannot describe that mutation.
fn refresh_embedding_projection(
    ctx: &Invocation<'_>,
    repo: &Repository,
    conn: &Connection,
    chunks: &[EmbeddableChunk],
    profile: &DeclaredEmbeddingProfile,
    projection: &EmbeddingProjection<'_>,
    held: &BTreeSet<String>,
) -> Result<()> {
    projection.policy.revalidate().map_err(pipeline_to_kio)?;
    let mut expected = BTreeMap::new();
    for chunk in chunks {
        if held.contains(&chunk.chunk_id) || !selected_embedding_member(chunk, projection) {
            continue;
        }
        if let Some(bytes) = embedding_store::content_vector(conn, chunk.work_key.embedding_hash())
            .map_err(index_to_kio)?
        {
            expected.insert(chunk.chunk_id.clone(), bytes);
        }
    }
    let existing = conn
        .prepare("SELECT chunk_id, embedding FROM chunk_vec")
        .map_err(|e| KioError::schema(e.to_string()))?
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .map_err(|e| KioError::schema(e.to_string()))?
        .collect::<std::result::Result<BTreeMap<_, _>, _>>()
        .map_err(|e| KioError::schema(e.to_string()))?;
    if expected == existing {
        if !embedding_replica_projection_is_current(repo) {
            projection.policy.revalidate().map_err(pipeline_to_kio)?;
            write_through_projection_or_log_for_repo(ctx, repo);
        }
        return Ok(());
    }
    // Invalidate readiness and cursor generation before the first scalar write.
    invalidate_embedding_projection(repo)?;
    let summary = embedding_store::EmbeddingProfileSummary {
        dimensions: profile.dimensions,
        distance: profile.distance.clone(),
        modality: profile.modality.clone(),
        profile_hash: profile.profile_hash.clone(),
    };
    embedding_store::rebuild_chunk_vec_for_contexts(
        conn,
        Some(&summary),
        held,
        projection.selected_contexts,
    )
    .map_err(index_to_kio)?;
    projection.policy.revalidate().map_err(pipeline_to_kio)?;
    write_through_projection_or_log_for_repo(ctx, repo);
    Ok(())
}

fn selected_embedding_member(
    chunk: &EmbeddableChunk,
    projection: &EmbeddingProjection<'_>,
) -> bool {
    projection.selected_contexts.get(&chunk.chunk_id)
        == Some(&embedding_store::chunk_embedding_context(&chunk.raw_path))
}

#[derive(Clone)]
struct EmbeddingSendGroup<'a> {
    embedding_hash: String,
    representative: &'a EmbeddableChunk,
    members: Vec<&'a EmbeddableChunk>,
}

/// Classify a batch into reuse vs. to-send by probing the content-addressed
/// `embeddings` store (CT3-EMBED-006). No writes, no adapter calls.
fn plan_embed_batch<'a>(
    conn: &Connection,
    profile: &DeclaredEmbeddingProfile,
    batch: &'a [EmbeddableChunk],
) -> std::result::Result<EmbedBatchPlan<'a>, TaskExecutionFailure> {
    let mut reuse = Vec::new();
    let mut missing = BTreeMap::<String, Vec<&EmbeddableChunk>>::new();
    for chunk in batch {
        let embedding_hash =
            chunk_embedding_hash(chunk, profile).map_err(|_| TaskExecutionFailure {
                retry_kind: RetryErrorKind::ContractViolation,
                retry_after_ms: None,
            })?;
        match embedding_store::content_vector(conn, &embedding_hash) {
            Ok(Some(bytes)) => reuse.push((chunk, bytes)),
            Ok(None) => missing.entry(embedding_hash).or_default().push(chunk),
            Err(_) => {
                return Err(TaskExecutionFailure {
                    retry_kind: RetryErrorKind::ContractViolation,
                    retry_after_ms: None,
                });
            }
        }
    }
    let to_send = missing
        .into_iter()
        .map(|(embedding_hash, members)| EmbeddingSendGroup {
            representative: members[0],
            embedding_hash,
            members,
        })
        .collect();
    Ok(EmbedBatchPlan { reuse, to_send })
}

/// Link `chunk_vec` for reuse hits (content vector already stored, no adapter).
fn link_reused_chunks(
    conn: &Connection,
    profile: &DeclaredEmbeddingProfile,
    reuse: &[(&EmbeddableChunk, Vec<u8>)],
    projection: &EmbeddingProjection<'_>,
    replica: &mut kio_index::aggregator::ScopeDelta,
) -> std::result::Result<(), TaskExecutionFailure> {
    validate_embedding_aliases(
        projection.policy,
        profile,
        reuse.iter().map(|(chunk, _)| *chunk),
    )?;
    for (chunk, bytes) in reuse {
        if !selected_embedding_member(chunk, projection) {
            continue;
        }
        invalidate_embedding_projection_once(projection).map_err(|_| TaskExecutionFailure {
            retry_kind: RetryErrorKind::ContractViolation,
            retry_after_ms: None,
        })?;
        let linked =
            embedding_store::link_chunk_vec(conn, &chunk.chunk_id, bytes, profile.dimensions)
                .map_err(|_| TaskExecutionFailure {
                    retry_kind: RetryErrorKind::ContractViolation,
                    retry_after_ms: None,
                })?;
        // A content-addressed reuse hit writes `chunk_vec` with no adapter call
        // and no rebuild, so it moves the vector corpus as surely as a send
        // does — and just as invisibly to a reader-driven refresh (05 §1.8).
        if linked {
            replica.vectors_added.push((
                chunk.chunk_id.clone(),
                embedding_store::f32_from_le_bytes(bytes),
            ));
        }
    }
    Ok(())
}

/// Send ONE group on the sync lane, write its `embeddings` + `chunk_vec`, and
/// return the usage the provider reported for that call.
///
/// One group per call is what makes the returned usage settleable as-is:
/// `:batchEmbedContents` reports a single `promptTokenCount` per CALL, so a
/// call spanning several ledger rows can only be divided among them by
/// guesswork. Both lanes now keep the billing unit and the accounting unit the
/// same object — the Batch lane by making the job the task (07 §5.5), this one
/// by making the call the row. The realtime lane is opt-in and never the
/// default (2026-07-25 ruling), so the round trips it costs are never on a
/// default path.
fn send_embed_group(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    conn: &Connection,
    embedding: &EmbeddingExecutionContext<'_>,
    group: &EmbeddingSendGroup<'_>,
    // QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880): this group's own
    // reservation token. A provider idempotency key identifies one HTTP call,
    // and one call is now one group, so there is nothing to pick a
    // representative from.
    idempotency_token: Option<String>,
    replica: &mut kio_index::aggregator::ScopeDelta,
    control: &ControlHandle,
) -> std::result::Result<Option<kio_adapter::types::AdapterUsage>, TaskExecutionFailure> {
    // Contextual-embedding addendum (07 §5.3, 2026-07-24): the adapter input is
    // the humanized filename context prepended to the chunk body. Every member
    // of a group shares one `embedding_hash` = one `(text_hash, context,
    // profile)`, so the representative's context is the whole group's context.
    validate_embedding_aliases(
        embedding.current_policy,
        embedding.profile,
        group.members.iter().copied(),
    )?;
    let context = embedding_store::chunk_embedding_context(&group.representative.raw_path);
    let items = vec![EmbeddingItem::text(
        group.embedding_hash.clone(),
        embedding_store::contextualized_embedding_input(
            context.as_deref(),
            &group.representative.text,
        ),
    )];
    // Online sends were admitted immediately before their ledger reservation;
    // local HTTP has no reservation, so admission must be at this call edge.
    if !embedding_is_online(embedding.execution) {
        admit_embedding_transport(
            adapter_settings,
            embedding.repo,
            embedding.execution,
            embedding.profile,
            embedding.current_policy,
            group
                .members
                .iter()
                .any(|chunk| chunk.requires_secret_approval),
        )
        .map_err(|_| TaskExecutionFailure {
            retry_kind: RetryErrorKind::AuthError,
            retry_after_ms: None,
        })?;
    }
    let outcome = run_embedding_adapter(
        adapter_settings,
        embedding.execution,
        items,
        EmbeddingInputType::MarkdownChunk,
        idempotency_token,
        control,
    )?;
    let Some(vector) = outcome
        .vectors
        .iter()
        .find(|vector| vector.id == group.embedding_hash)
    else {
        return Err(TaskExecutionFailure {
            retry_kind: RetryErrorKind::ContractViolation,
            retry_after_ms: None,
        });
    };
    persist_group_vector(
        conn,
        embedding.profile,
        group,
        &vector.vector,
        &BTreeSet::new(),
        &embedding.projection,
        replica,
    )?;
    Ok(outcome.usage)
}

/// Write one group's content vector and fan it out to every member chunk.
///
/// Shared by BOTH lanes: the sync path calls it with the vector the adapter
/// just returned, the Batch path with the vector collected from a finished
/// provider job. 07 §5.3's acceptance checks must not exist in two versions,
/// so neither may they diverge here.
///
/// `replica` accumulates what to write through to the device replica (05 §1.8).
/// It is threaded down to here — rather than reconstructed by each lane — for
/// the same reason the acceptance checks are: this is the single point that
/// knows which chunks actually gained a vector, and both lanes write into the
/// LIVE index without rebuilding it, so nothing downstream re-reads the scope.
/// The caller flushes once, with `write_through_delta`.
fn persist_group_vector(
    conn: &Connection,
    profile: &DeclaredEmbeddingProfile,
    group: &EmbeddingSendGroup<'_>,
    vector: &[f32],
    held: &BTreeSet<String>,
    projection: &EmbeddingProjection<'_>,
    replica: &mut kio_index::aggregator::ScopeDelta,
) -> std::result::Result<(), TaskExecutionFailure> {
    let repo = projection.repo;
    let chunk = group.representative;
    if chunk_embedding_hash(chunk, profile).map_err(|_| TaskExecutionFailure {
        retry_kind: RetryErrorKind::ContractViolation,
        retry_after_ms: None,
    })? != group.embedding_hash
        || chunk.work_key.embedding_hash() != group.embedding_hash
    {
        return Err(TaskExecutionFailure {
            retry_kind: RetryErrorKind::ContractViolation,
            retry_after_ms: None,
        });
    }
    let bytes = f32_to_le_bytes(vector);
    let context_key = embedding_store::chunk_embedding_context(&chunk.raw_path);
    // CAS first, table second (04 §4.3: `objects/` → `embeddings` → `chunk_vec`).
    // The order is what makes a crash between them recoverable in the direction
    // that matters: an object with no row is replayed by the next rebuild, while
    // a row with no object is a vector `rebuild-db` cannot restore.
    publish_embedding_object(
        repo,
        &group.embedding_hash,
        &chunk.text_hash,
        vector,
        profile,
        context_key.as_deref(),
    )
    .map_err(|_| TaskExecutionFailure {
        retry_kind: RetryErrorKind::ContractViolation,
        retry_after_ms: None,
    })?;
    projection
        .policy
        .revalidate()
        .map_err(|_| TaskExecutionFailure {
            retry_kind: RetryErrorKind::AuthError,
            retry_after_ms: None,
        })?;
    invalidate_embedding_projection_once(projection).map_err(|_| TaskExecutionFailure {
        retry_kind: RetryErrorKind::ContractViolation,
        retry_after_ms: None,
    })?;
    embedding_store::write_chunk_embedding_source(
        conn,
        &group.embedding_hash,
        &chunk.text_hash,
        &bytes,
        profile.dimensions,
        &profile.distance,
        &profile.modality,
        &profile.profile_hash,
        context_key.as_deref(),
    )
    .map_err(|_| TaskExecutionFailure {
        retry_kind: RetryErrorKind::ContractViolation,
        retry_after_ms: None,
    })?;
    validate_embedding_aliases(projection.policy, profile, group.members.iter().copied())?;
    let linked = embedding_store::link_chunk_vecs_to_content_vector(
        conn,
        &group.embedding_hash,
        group
            .members
            .iter()
            .filter(|member| selected_embedding_member(member, projection))
            .map(|member| member.chunk_id.as_str()),
        held,
    )
    .map_err(|_| TaskExecutionFailure {
        retry_kind: RetryErrorKind::ContractViolation,
        retry_after_ms: None,
    })?;
    // The ids the link REPORTED, never `group.members`. A secrets-held member
    // (R20-10) is deliberately kept out of `chunk_vec` so it cannot be reached
    // by vector search without `--send-secrets`, and a width-mismatched member
    // is dropped as incompatible. Mirroring either rule here would put it in
    // two places, and the day they drifted the replica would expose a held
    // chunk (03 §4 invariant 8).
    replica.vectors_added.extend(
        linked
            .into_iter()
            .map(|chunk_id| (chunk_id, vector.to_vec())),
    );
    Ok(())
}

// ===========================================================================
// Embedding Batch lane (07 §5.3 の 2026-07-24 訂正 / §5.7, 04 §5.8)
//
// The Gemini Developer API bills an embedding batch at half the sync rate, so
// the Batch lane is the default. Unlike the OCR lane it takes its input
// INLINE, so there is no upload phase (相 2a) and no provider residue to
// sweep — see `kio_adapter::gemini_batch_client`.
//
// **1 job = 1 task** (07 §5.5's v1 contract) is preserved by making the JOB
// itself the task: the batch row's `input_hash` is a digest of the job's
// member set. Embedding's natural task granularity is the dedup group, and a
// realistic scope has thousands of them (2,321 in the dogfood fixture), so
// one job per group would mean thousands of provider jobs. One row per job
// keeps the §5.8 state machine, its crash recovery, and `kio batch abandon`
// working unchanged.
// ===========================================================================

/// Groups carried by one embedding batch job. Well under the adapter's inline
/// bounds (`MAX_INLINE_REQUESTS` = 2048, 16 MiB serialized), leaving room for
/// long chunks: 6,000 chars max per chunk (`[chunking] max_chars`) × 512 ≈ 3 MB
/// of text before JSON overhead.
const EMBEDDING_BATCH_JOB_MAX_MEMBERS: usize = 512;

/// How many non-success terminals one embedding batch task (= one job's member
/// set) may accumulate before it is failed instead of re-sent.
///
/// R24: a job that keeps ending in a non-success terminal state used to be
/// re-submitted without bound — every `batch resume` settled the failure,
/// cleared the row, and immediately created a new job with a fresh reservation.
/// A failed send reports no usage — measured settlement (I4/I12) covers the
/// success path only — so on this path the reservation IS the settled charge,
/// and a permanently-failing member set recorded its full estimate once per
/// pass until the budget cap hard-stopped the scope.
///
/// Counted on `contract_violation_count`, NOT on `attempts`, specifically
/// because **`kio batch retry --reset-violations <selector> --yes` already exists as the operator escape
/// hatch for it** (CL62-CL68) and nothing resets `attempts`. Bounding on
/// `attempts` would have made an exhausted member set permanently
/// unsubmittable — a dead end with no way out. 07 §5.3 already routes a
/// response that yields no usable output through the contract-violation rules,
/// so a FAILED/EXPIRED/CANCELLED job counting here is the established meaning.
const EMBEDDING_BATCH_JOB_MAX_FAILURES: i64 = 3;

/// G2: record one Batch-submit provider failure against the job's member
/// chunks, using the same classification and the same 3-way disposition the
/// synchronous send handler applies.
///
/// The reservation is deliberately LEFT IN PLACE (the F8 reserve-before-send
/// posture): whether the provider actually created the job is unknown from
/// here, so releasing it could double-charge on the next pass. 04 §5.8's
/// recovery walk resolves it — found → the poll lane collects it, confirmed
/// absent → the reservation is released.
fn record_batch_submit_failure(
    outcome: &mut ExecOutcome,
    transitions: &mut BTreeMap<String, EmbeddingTransition>,
    job_groups: &[EmbeddingSendGroup<'_>],
    error: kio_adapter::AdapterError,
) {
    let failure = task_failure_from_adapter(error);
    record_embedding_transitions(
        transitions,
        job_groups
            .iter()
            .flat_map(|group| group.members.iter().copied()),
        embedding_failure_transition(&failure),
    );
    count_embedding_failure(
        outcome,
        failure.retry_kind,
        job_groups.iter().map(|group| group.members.len()).sum(),
    );
}

/// Total `promptTokenCount` a job's result lines reported, or `None` when not
/// every line carried one.
///
/// All-or-nothing on purpose. A partial sum would silently under-state the
/// charge, and the ledger's own rule (CL28) is that an incomplete usage report
/// degrades to the conservative reservation rather than being trusted in part.
/// Returning `None` is what routes it there.
fn reported_prompt_tokens(
    results: &[kio_adapter::gemini_batch_client::GeminiBatchEmbedOutput],
) -> Option<u64> {
    if results.is_empty() {
        return None;
    }
    results
        .iter()
        .map(|result| result.prompt_tokens)
        .try_fold(0u64, |total, tokens| Some(total + tokens?))
}

/// The batch row's `input_hash` for a job: a digest over the job's member
/// content identities, in a canonical order.
///
/// This is what makes "1 job = 1 task" hold for a lane whose task granularity
/// is finer than a job. Two runs that select the same member set land on the
/// same row (so a crash-stranded reservation is reused, not duplicated); a
/// different selection is a different task, and any member already embedded by
/// the earlier job drops out through content-addressed reuse before it is ever
/// selected again.
const EMBEDDING_BATCH_RESULT_CONTRACT_ERROR: &str =
    "embedding batch result violates the complete response contract";

fn embedding_batch_results_match_job(
    results: &[kio_adapter::gemini_batch_client::GeminiBatchEmbedOutput],
    input_hash: &str,
) -> bool {
    let keys = results
        .iter()
        .map(|result| result.key.clone())
        .collect::<BTreeSet<_>>();
    !results.is_empty()
        && keys.len() == results.len()
        && results
            .iter()
            .all(|result| is_hash(&result.key) && result.values.is_some() && result.error.is_none())
        && embedding_job_input_hash(&keys) == input_hash
}

fn embedding_job_input_hash(embedding_hashes: &BTreeSet<String>) -> String {
    let joined = embedding_hashes
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    kio_core::cas::hash_bytes(joined.as_bytes())
}

/// Validate the entire response before the caller can persist its first line.
/// An old profile is settlement-only; never reinterpret its dimensions under
/// the currently configured vector space.
fn validated_embedding_batch_vectors(
    results: &[kio_adapter::gemini_batch_client::GeminiBatchEmbedOutput],
    input_hash: &str,
    current_profile: Option<&DeclaredEmbeddingProfile>,
) -> Result<BTreeMap<String, Vec<f32>>> {
    if !embedding_batch_results_match_job(results, input_hash) {
        return Err(KioError::schema(EMBEDDING_BATCH_RESULT_CONTRACT_ERROR));
    }
    let Some(profile) = current_profile else {
        return Ok(BTreeMap::new());
    };
    results
        .iter()
        .map(|result| {
            normalize_embedding_vector(
                result
                    .values
                    .as_deref()
                    .expect("complete result was checked"),
                profile,
            )
            .map(|vector| (result.key.clone(), vector))
        })
        .collect()
}

/// 07 §5.3 (3)/(4): validate the provider's raw values, convert deterministically
/// to unit-norm f32, then re-validate the RESULT — an underflowed zero vector or
/// an overflowed infinity must never reach the index. The sync lane gets this
/// inside the adapter; the Batch lane's values arrive raw from a job, so the
/// same discipline is applied here.
fn normalize_embedding_vector(
    values: &[f32],
    profile: &DeclaredEmbeddingProfile,
) -> Result<Vec<f32>> {
    let dimensions = u32::try_from(profile.dimensions).unwrap_or(u32::MAX);
    kio_adapter::types::validate_cosine_vector(values, dimensions).map_err(adapter_to_kio)?;
    let norm = values
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>()
        .sqrt();
    let normalized = values
        .iter()
        .map(|value| (f64::from(*value) / norm) as f32)
        .collect::<Vec<f32>>();
    kio_adapter::types::validate_cosine_vector(&normalized, dimensions).map_err(adapter_to_kio)?;
    Ok(normalized)
}

/// Submit this pass's pending embeddings as provider batch jobs and return.
///
/// Nothing is collected here: the results arrive at a later `poll_batch_embedding_jobs`
/// (from `kio batch resume` or any write command), the same shape the OCR lane
/// uses. Member tasks stay Pending, so `index_status` keeps reporting them.
#[allow(clippy::too_many_arguments)]
fn submit_embedding_batch_jobs(
    ctx: &Invocation<'_>,
    repo: &Repository,
    execution: EmbeddingExecution,
    conn: &Connection,
    ledger: &LedgerDb,
    profile: &DeclaredEmbeddingProfile,
    embeddable: &[EmbeddableChunk],
    scope_id: &str,
    budget_caps: &BudgetCaps,
    override_budget: bool,
    current_policy: &CurrentPolicyEvaluator,
    projection: &EmbeddingProjection<'_>,
    transitions: &mut BTreeMap<String, EmbeddingTransition>,
    replica: &mut kio_index::aggregator::ScopeDelta,
) -> Result<Option<ExecOutcome>> {
    let adapter_settings = ctx.settings;

    let mut outcome = ExecOutcome::default();
    let contract_holds = kio_pipeline::ledger::ops::unreset_contract_violations_for_scope_adapter(
        ledger,
        scope_id,
        EMBEDDING_ADAPTER_KIND,
    )
    .map_err(pipeline_to_kio)?;
    let plan = plan_embed_batch(conn, profile, embeddable).map_err(|failure| {
        KioError::schema(format!(
            "embedding batch planning failed: {:?}",
            failure.retry_kind
        ))
    })?;
    // The durable batch breaker precedes client resolution: losing the batch
    // client must not turn a blocked batch into an automatic sync resend.
    if !contract_holds.is_empty() && !plan.to_send.is_empty() {
        let attempts = contract_holds
            .iter()
            .map(|row| {
                json!({
                    "input_hash": row.key.input_hash,
                    "tool_profile_hash": row.key.tool_profile_hash,
                    "submission_seq": row.submission_seq,
                    "selector": format!("{}/{}/{}/{}", row.key.scope_id, row.key.adapter_kind, row.key.input_hash, row.key.tool_profile_hash),
                })
            })
            .collect::<Vec<_>>();
        return Err(KioError::new(
            "KIO-E-EMBED-BATCH-CONTRACT-HOLD-001",
            "new embedding batch submissions are held after a malformed result; run kio batch retry --reset-violations <selector> --yes for each blocked attempt",
            json!({"scope_id": scope_id, "adapter_kind": EMBEDDING_ADAPTER_KIND, "blocked_attempts": attempts}),
            ExitCode::PermanentFailure,
        ));
    }
    // The lazy context is loaded only for a configured real client. Invalid
    // recovery authority is a hard hold, never permission to switch lanes.
    let Some(client) = kio_adapter::gemini_batch_client::resolve_gemini_batch_client_with_control(
        adapter_settings,
        || batch_recovery_context(ledger),
        ctx.control,
    )
    .map_err(adapter_to_kio)?
    else {
        // The lane is unavailable (no batch client resolvable). Report that to
        // the caller so it can fall back to the synchronous lane, exactly as
        // `markdownize_send_lane`'s `(false, None) => Sync` rule does — leaving
        // the work pending forever would be worse than paying the sync rate.
        return Ok(None);
    };
    // Reuse links are free and always succeed — settle them here exactly as the
    // sync lane does, so an unavailable provider cannot strand an already
    // materialized chunk.
    if !plan.reuse.is_empty() {
        match link_reused_chunks(conn, profile, &plan.reuse, projection, replica) {
            Ok(()) => {
                record_embedding_transitions(
                    transitions,
                    plan.reuse.iter().map(|(chunk, _)| *chunk),
                    embedding_done_transition(),
                );
                outcome.executed += plan.reuse.len();
            }
            Err(failure) => {
                record_embedding_transitions(
                    transitions,
                    plan.reuse.iter().map(|(chunk, _)| *chunk),
                    embedding_fail_transition(failure.retry_kind),
                );
                count_embedding_failure(&mut outcome, failure.retry_kind, plan.reuse.len());
                return Ok(Some(outcome));
            }
        }
    }
    if plan.to_send.is_empty() {
        return Ok(Some(outcome));
    }
    let caps = budget_cap_config(budget_caps, scope_id, EMBEDDING_ADAPTER_KIND);
    let bypass_cap_denial = override_budget || !budget_caps.hard_stop;
    for job_groups in plan.to_send.chunks(EMBEDDING_BATCH_JOB_MAX_MEMBERS) {
        let member_identities = job_groups
            .iter()
            .map(|group| group.embedding_hash.clone())
            .collect::<BTreeSet<_>>();
        let key = task_ledger_key(
            scope_id,
            EMBEDDING_ADAPTER_KIND,
            &embedding_job_input_hash(&member_identities),
            &profile.profile_hash,
        );
        // R24 (6 系統一致, KNOWN GAP の是正): bound the retries. A job that ends
        // in a non-success terminal settles the row and NULLs its
        // `intent_token`, and `phase1_intent`'s ON CONFLICT clears
        // `batch_job_id` — so without this gate the very next pass re-reserved
        // the same task key and created another job, forever, recording an
        // estimate every time. Neither counter is in that ON CONFLICT SET list,
        // so both survive re-reservation; this uses `contract_violation_count`
        // because `kio batch retry --reset-violations <selector> --yes` can clear it and nothing clears
        // `attempts` (see `EMBEDDING_BATCH_JOB_MAX_FAILURES`).
        let existing = get_batch_request(ledger, &key).map_err(pipeline_to_kio)?;
        if existing.as_ref().is_some_and(|row| {
            !row.state.is_inflight()
                && row.contract_violation_count >= EMBEDDING_BATCH_JOB_MAX_FAILURES
        }) {
            record_embedding_transitions(
                transitions,
                job_groups
                    .iter()
                    .flat_map(|group| group.members.iter().copied()),
                embedding_fail_transition(RetryErrorKind::ContractViolation),
            );
            count_embedding_failure(
                &mut outcome,
                RetryErrorKind::ContractViolation,
                job_groups
                    .iter()
                    .map(|group| group.members.len())
                    .sum::<usize>(),
            );
            continue;
        }
        // This is an already-admitted request. Its poll/recovery owner may
        // continue after a policy change; do not turn a resubmission scan into
        // a fresh admission boundary for it.
        if existing
            .as_ref()
            .is_some_and(|row| row.state.is_inflight() && row.batch_job_id.is_some())
        {
            continue;
        }
        // R24 (3 系統一致): build the inputs BEFORE the reservation so the
        // estimate prices the string actually sent. Estimating
        // `representative.text` while sending
        // `contextualized_embedding_input(context, text)` undercounts by the
        // whole context prefix. Settlement now corrects that from the reported
        // tokens (I4/I12), but the RESERVATION is what the budget cap gates on
        // and what a failed send settles at, so an undercount here still lets
        // spending through the cap that the cap was set to stop (I10).
        let inputs = job_groups
            .iter()
            .map(|group| {
                let context =
                    embedding_store::chunk_embedding_context(&group.representative.raw_path);
                kio_adapter::gemini_batch_client::GeminiBatchEmbedInput {
                    // The provider echoes this back in `metadata.key`; the
                    // collect side keys on it to find the chunk again.
                    key: group.embedding_hash.clone(),
                    text: embedding_store::contextualized_embedding_input(
                        context.as_deref(),
                        &group.representative.text,
                    ),
                }
            })
            .collect::<Vec<_>>();
        validate_embedding_aliases(
            current_policy,
            profile,
            job_groups
                .iter()
                .flat_map(|group| group.members.iter().copied()),
        )
        .map_err(|_| KioError::schema("embedding owner policy changed before batch admission"))?;
        // One reservation for the whole job — the cap is checked once against
        // the job's total, which is also what the provider will bill.
        let candidate_usd: f64 = inputs
            .iter()
            .map(|input| {
                estimate_embedding_cost(adapter_settings, &input.text, PreferredRequestKind::Batch)
            })
            .sum();
        // Final check before this new job's reservation/admission. Existing
        // submitted jobs are polled elsewhere and deliberately do not pass
        // through this new-submission boundary.
        admit_embedding_transport(
            adapter_settings,
            repo,
            execution,
            profile,
            current_policy,
            job_groups
                .iter()
                .flat_map(|group| group.members.iter())
                .any(|chunk| chunk.requires_secret_approval),
        )?;
        let charge = reserve_or_reuse_task_charge(
            ctx,
            ledger,
            &key,
            candidate_usd,
            &caps,
            bypass_cap_denial,
            RequestKind::Batch,
        )?;
        let (intent_token, _reserved_usd) = match charge {
            TaskChargeOutcome::BudgetExceeded => {
                record_embedding_transitions(
                    transitions,
                    job_groups
                        .iter()
                        .flat_map(|group| group.members.iter().copied()),
                    embedding_pause_transition(),
                );
                outcome.paused += job_groups
                    .iter()
                    .map(|group| group.members.len())
                    .sum::<usize>();
                continue;
            }
            TaskChargeOutcome::Reserved {
                intent_token,
                estimated_usd,
            }
            | TaskChargeOutcome::Reused {
                intent_token,
                estimated_usd,
            } => (intent_token, estimated_usd),
        };
        // 相 2a is degenerate for an inline batch (no upload), so the provider
        // scope is recorded immediately before job creation instead.
        //
        // G2: a provider-side failure here fails THIS job, not the invocation.
        // These calls used to `?` straight out of the function, so one
        // transient 429 aborted `kio index` for the whole scope — and, worse,
        // did it after 相 2b was recorded, leaving the row stranded. The
        // synchronous lane has always classified and recorded per chunk
        // instead; this makes the Batch lane behave the same way. The stranded
        // row is not a leak: §5.8's recovery walk owns it (G1 wired the Gemini
        // provider into that walk, which is why this can safely give up here).
        let provider_scope_id = match client.provider_scope_id() {
            Ok(id) => id,
            Err(error) => {
                record_batch_submit_failure(&mut outcome, transitions, job_groups, error);
                continue;
            }
        };
        if !phase2a_record_provider_scope(ledger, &key, &intent_token, &provider_scope_id)
            .map_err(pipeline_to_kio)?
        {
            continue;
        }
        if !phase2b_record_job_create_started(ledger, &key, &intent_token)
            .map_err(pipeline_to_kio)?
        {
            continue;
        }
        let job = match client.create_embedding_job(
            kio_adapter::catalog::ADOPTED_EMBEDDING_MODEL_PIN,
            u32::try_from(profile.dimensions).unwrap_or(u32::MAX),
            &kio_adapter::gemini_batch_client::batch_display_name(&intent_token),
            &inputs,
        ) {
            Ok(job) => job,
            Err(error) => {
                record_batch_submit_failure(&mut outcome, transitions, job_groups, error);
                continue;
            }
        };
        phase2b_record_job_created(ledger, &key, &intent_token, &job.name)
            .map_err(pipeline_to_kio)?;
        outcome.inflight += job_groups
            .iter()
            .map(|group| group.members.len())
            .sum::<usize>();
    }
    Ok(Some(outcome))
}

/// 04 §5.8 相 3 for this scope's in-flight embedding batch jobs.
///
/// Polling receives an EXISTING request, so like the OCR lane it needs no
/// network opt-in. Rows whose job id is still unknown belong to the §5.8
/// recovery walk (`kio ledger reconcile`), not here — `batch_poll_candidates`
/// only returns rows with a durable `batch_job_id`.
fn poll_batch_embedding_jobs(
    ctx: &Invocation<'_>,
    repo: &Repository,
    ledger: &LedgerDb,
) -> Result<ExecOutcome> {
    let adapter_settings = ctx.settings;

    let mut outcome = ExecOutcome::default();
    let Some(execution) = embedding_execution(adapter_settings, ctx.control) else {
        return Ok(outcome);
    };
    // 07 §5.5: the lane is a property of the adapter. An `offline_api` adapter
    // prefers Sync because a local server has no provider job queue to submit
    // to, and the half-price rationale that puts Gemini on Batch does not
    // exist for it. There can be no in-flight rows to poll.
    if embedding_preferred_request_kind_with_control(adapter_settings, execution, ctx.control)
        .map_err(adapter_to_kio)?
        != PreferredRequestKind::Batch
    {
        return Ok(outcome);
    }
    let profile = declared_embedding_profile(execution);
    // The lazy context is loaded only for a configured real client. Invalid
    // recovery authority is a hard hold, never permission to switch lanes.
    let Some(client) = kio_adapter::gemini_batch_client::resolve_gemini_batch_client_with_control(
        adapter_settings,
        || batch_recovery_context(ledger),
        ctx.control,
    )
    .map_err(adapter_to_kio)?
    else {
        return Ok(outcome);
    };
    let scope_id = repo.scope_identity()?.scope_id;
    recover_terminal_embedding_batch_cleanup(ctx, ledger, client.as_ref(), &scope_id)?;
    let rows = batch_poll_candidates(ledger, &scope_id, EMBEDDING_ADAPTER_KIND)
        .map_err(pipeline_to_kio)?;
    if rows.is_empty() {
        return Ok(outcome);
    }
    let db_path = sqlite_path(repo.kio_dir());
    if !db_path.exists() {
        return Ok(outcome);
    }
    let conn = open_existing_source_index_connection(
        &db_path,
        ExistingSourceIndexOpenMode::ReadWrite,
        &FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        },
    )
    .map_err(index_to_kio)?;
    let Some(head) = repo.head_commit_hash()? else {
        return Ok(outcome);
    };
    // Batch collection is likewise scoped by the required HEAD TreeObject
    // field, never by a mutable working-tree config.toml.
    let instances = retained_history_instances(repo, &head)?;
    let chunks =
        embedding_owners::collect_retained_embedding_chunks(repo, &conn, &instances, None, None)?;
    let current_policy = current_embedding_policy(repo)?;
    let selected_contexts = selected_embedding_contexts(&chunks, &current_policy)?;
    let chunks = bind_embedding_work(chunks, &profile)?;
    let by_embedding_hash = chunks
        .iter()
        .map(|chunk| (chunk.work_key.embedding_hash().to_owned(), chunk))
        .collect::<BTreeMap<_, _>>();
    let mut held = BTreeSet::new();
    if secret_embedding_hold_required(
        execution.execution_mode(),
        embedding_secrets_allowed(adapter_settings, repo, &profile)?,
        true,
    ) {
        held.extend(held_secret_embedding_chunk_ids(
            repo.kio_dir(),
            repo.control(),
        )?);
        held.extend(
            chunks
                .iter()
                .filter(|chunk| chunk.requires_secret_approval)
                .map(|chunk| chunk.chunk_id.clone()),
        );
    }
    let projection = EmbeddingProjection {
        repo,
        policy: &current_policy,
        selected_contexts: &selected_contexts,
        invalidated: std::cell::Cell::new(false),
    };
    refresh_embedding_projection(ctx, repo, &conn, &chunks, &profile, &projection, &held)?;
    // 05 §1.8 write-through, accumulated across every collected job and flushed
    // once below — see the sync lane's identical accumulator, including why the
    // generation is read here rather than at the flush.
    let replica_before = replica_scope_stamp(repo.kio_dir()).map(|(_, generation)| generation);
    let mut replica = kio_index::aggregator::ScopeDelta::default();
    let mut source_written = false;
    for row in rows {
        let (Some(job_name), Some(intent_token)) =
            (row.batch_job_id.as_ref(), row.intent_token.as_ref())
        else {
            continue;
        };
        // G2: 04 §5.8 unknown — 何も変更せず保持し、次回再試行する。This used to
        // `?` out of the whole function, so ONE unreachable row blocked
        // collection for every other row in the scope (head-of-line blocking)
        // and reported a transient network error as a permanent config fault.
        // The OCR lane has always held-and-retried here; this matches it.
        // I8: ONE fetch. The state and the results live in the same document,
        // so asking twice downloaded a finished job's payload twice and let the
        // two reads take different byte bounds — which is how every job past
        // ~20 members came to be silently unreadable.
        let Some(poll) = poll_attributed_embedding_batch_job(ctx, client.as_ref(), &row, None)
        else {
            outcome.inflight += 1;
            outcome.held_unreadable += 1;
            continue;
        };
        let job = poll.record;
        if !job.state.is_terminal() {
            outcome.inflight += 1;
            continue;
        }
        if job.state != kio_adapter::gemini_batch_client::GeminiBatchState::Succeeded {
            // Failed / cancelled / expired: terminate the row so the members
            // become eligible for a fresh submission, and record the estimate
            // (the provider may still have billed part of the job).
            settle_batch_charge_terminal(
                ledger,
                &row.key,
                intent_token,
                job_name,
                // §5.8 相 3: a provider-side non-success terminal. `Expired`
                // is the closed-enum outcome for "the job ended without
                // usable output" (the OCR lane uses it the same way).
                Outcome::Expired,
                BatchState::Terminal,
                Some("embedding batch job did not succeed"),
                BilledAmount {
                    usd: row.estimated_usd,
                    estimated: true,
                },
                // R24: count it. This is the durable counter the submit side
                // bounds re-sends on, and `kio batch retry --reset-violations <selector> --yes` is its
                // operator escape hatch.
                true,
                true,
            )?;
            outcome.failed += 1;
            continue;
        }
        // G2: same §5.8 unknown rule. The job says SUCCEEDED but the response
        // carried no results — settling on that would either discard the
        // vectors (Succeeded with nothing persisted) or burn a retry
        // (Terminal). Hold the row instead; the next pass re-reads it.
        let Some(results) = poll.results else {
            ctx.interaction.diagnostic(&format!(
                "kio: holding {job_name}: job succeeded but carried no results"
            ));
            outcome.inflight += 1;
            outcome.held_unreadable += 1;
            continue;
        };
        // The row binds only a digest of the original member set. Establish
        // the complete bijection before the first CAS or source-table write.
        let validated_vectors = match validated_embedding_batch_vectors(
            &results,
            &row.key.input_hash,
            (row.key.tool_profile_hash == profile.profile_hash).then_some(&profile),
        ) {
            Ok(vectors) => vectors,
            Err(_) => {
                settle_batch_charge_terminal(
                    ledger,
                    &row.key,
                    intent_token,
                    job_name,
                    Outcome::ContractViolation,
                    BatchState::Terminal,
                    Some(EMBEDDING_BATCH_RESULT_CONTRACT_ERROR),
                    BilledAmount {
                        usd: row.estimated_usd,
                        estimated: true,
                    },
                    true,
                    true,
                )?;
                outcome.failed += 1;
                continue;
            }
        };
        let mut persisted = 0usize;
        // R24 (4 系統一致): 07 §5.3 (1) requires the result ids to be a BIJECTION
        // onto the input ids, and this loop checked neither direction — it
        // iterated the results and `continue`d past anything it could not
        // resolve, then settled `Succeeded` unconditionally. A provider that
        // returned 511 of 512 embeddings therefore completed the row, cleared
        // its `intent_token`, and left the missing chunk with no vector and no
        // failed task: a silent, permanent hole in the index.
        //
        // The bijection is checkable with no schema change because the task key
        // IS the member digest (design A): re-derive the digest from the
        // results that actually resolved and compare it with the row's own
        // `input_hash`.
        for result in &results {
            let Some(chunk) = by_embedding_hash.get(&result.key) else {
                // The chunk is gone (purged / reconfigured) since submission —
                // nothing to write, and nothing to fail either.
                continue;
            };
            let Some(normalized) = validated_vectors.get(&result.key) else {
                continue;
            };
            let embedding_hash = chunk_embedding_hash(chunk, &profile)?;
            // Re-derive the group from the CURRENT chunk set rather than
            // trusting the submitted membership: a chunk added since
            // submission that shares this content identity must be linked too.
            let members = chunks
                .iter()
                .filter(|candidate| {
                    candidate.work_key.embedding_hash() == embedding_hash
                        && current_policy
                            .allows_path(&candidate.raw_path)
                            .unwrap_or(false)
                })
                .collect::<Vec<_>>();
            let group = EmbeddingSendGroup {
                embedding_hash,
                representative: chunk,
                members,
            };
            persist_group_vector(
                &conn,
                &profile,
                &group,
                normalized,
                &held,
                &projection,
                &mut replica,
            )
            .map_err(|failure| {
                KioError::schema(format!(
                    "embedding batch collect failed to persist: {:?}",
                    failure.retry_kind
                ))
            })?;
            source_written = true;
            persisted += group.members.len();
        }
        // Membership was authenticated before persistence. A retired profile
        // or denied context can settle under its original job binding without
        // becoming a current live vector.
        let reported = reported_prompt_tokens(&results).map(|tokens| {
            kio_adapter::types::AdapterUsage::BillableUnits {
                billable_units: vec![kio_adapter::types::BillableUnit {
                    kind: kio_adapter::types::BillableUnitKind::TokensIn,
                    count: tokens,
                }],
            }
        });
        settle_batch_charge_terminal(
            ledger,
            &row.key,
            intent_token,
            job_name,
            Outcome::Succeeded,
            BatchState::Completed,
            None,
            embedding_billed_from_usage(
                adapter_settings,
                reported.as_ref(),
                PreferredRequestKind::Batch,
                row.estimated_usd,
                ctx.control,
                ctx.locks,
            ),
            false,
            true,
        )?;
        outcome.executed += persisted;
    }
    // A collected batch writes vectors straight into the LIVE index, and
    // `run_batch` — unlike `run_index`/`run_reindex` — never rebuilds it
    // afterward, so `index_generation` would otherwise still name the
    // pre-embedding state. That retires every outstanding search cursor, which
    // is what LC25 requires of any change that can make a replay rank
    // differently. `publish_in_place_delta` owns both halves — it rotates and
    // then stamps the replica with what the index now carries.
    //
    // Its empty-delta no-op replaces the former `outcome.executed > 0` test,
    // and narrows it: a group whose every member is secrets-held writes a
    // `content_vectors` row (so `executed` counts it) but links nothing into
    // `chunk_vec`, and search reads `chunk_vec`. No replay can rank
    // differently, so LC25 asks for no rotation — the old trigger retired
    // cursors for a change no search could see.
    current_policy.revalidate().map_err(pipeline_to_kio)?;
    publish_in_place_delta(ctx, repo.kio_dir(), replica_before.as_deref(), &replica)?;
    if source_written && replica.is_empty() {
        write_through_projection_or_log_for_repo(ctx, repo);
    }
    Ok(outcome)
}

/// Provider scope is the configured local identity, not a network lookup.
/// Missing, unreadable, or changed authority never permits recovery effects.
fn batch_provider_scope_matches(
    stored: Option<&str>,
    current: impl FnOnce() -> kio_adapter::Result<String>,
) -> bool {
    let Some(stored) = stored.filter(|scope| !scope.is_empty()) else {
        return false;
    };
    current().is_ok_and(|current| current == stored)
}

/// Both collection and terminal cleanup use the same exact Gemini attribution.
/// Collection additionally binds the active vector profile before any poll.
fn poll_attributed_embedding_batch_job<
    C: kio_adapter::gemini_batch_client::GeminiBatchClient + ?Sized,
>(
    ctx: &Invocation<'_>,
    client: &C,
    row: &BatchRequestRow,
    required_profile: Option<&str>,
) -> Option<kio_adapter::gemini_batch_client::GeminiBatchPoll> {
    if required_profile.is_some_and(|profile| row.key.tool_profile_hash != profile)
        || !batch_provider_scope_matches(row.provider_scope_id.as_deref(), || {
            client.provider_scope_id()
        })
    {
        ctx.interaction
            .diagnostic("kio: holding embedding batch: provider scope or profile does not match");
        return None;
    }
    let job_name = row.batch_job_id.as_deref()?;
    let intent_token = row.intent_token.as_deref()?;
    let poll = match client.poll_job(job_name) {
        Ok(poll) => poll,
        Err(error) => {
            ctx.interaction.diagnostic(&format!(
                "kio: holding {job_name}: poll unreadable: {error}"
            ));
            return None;
        }
    };
    if poll.record.name != job_name
        || poll.record.display_name
            != kio_adapter::gemini_batch_client::batch_display_name(intent_token)
    {
        ctx.interaction.diagnostic(&format!(
            "kio: holding {job_name}: returned job attribution does not match"
        ));
        return None;
    }
    Some(poll)
}

/// Finish only the narrow, no-residue cleanup case for an already-terminal
/// Gemini embedding job. This never creates, cancels, or resubmits a job.
fn recover_terminal_embedding_batch_cleanup<
    C: kio_adapter::gemini_batch_client::GeminiBatchClient + ?Sized,
>(
    ctx: &Invocation<'_>,
    ledger: &LedgerDb,
    client: &C,
    scope_id: &str,
) -> Result<()> {
    for row in kio_pipeline::ledger::ops::recovery_candidates(ledger).map_err(pipeline_to_kio)? {
        if row.key.scope_id != scope_id
            || row.key.adapter_kind != EMBEDDING_ADAPTER_KIND
            || !row.state.is_terminal()
        {
            continue;
        }
        let Some(intent_token) = row.intent_token.as_deref() else {
            continue;
        };
        let Some(poll) = poll_attributed_embedding_batch_job(ctx, client, &row, None) else {
            continue;
        };
        let job = poll.record;
        let job_name = &job.name;
        if !job.state.is_terminal() || job.responses_file.is_some() {
            ctx.interaction.diagnostic(&format!(
                "kio: holding {job_name}: terminal cleanup attribution or output is not safe"
            ));
            continue;
        }
        recovery_finish_cleanup(ledger, &row.key, intent_token).map_err(pipeline_to_kio)?;
    }
    Ok(())
}

/// L2(ii)/L7 target selection: drop chunks whose embedding task must not run in
/// this pass — a sticky budget-Paused task (unless `override_budget`), a Failed
/// task that is not retry-eligible (unelapsed `next_retry_at`, or non-retryable /
/// attempts exhausted), or (QA3, step4b-contract-tests-p3a.md §A) a rate-limited
/// Pending task whose `next_retry_at` backoff has not yet elapsed. Failed
/// retry-eligible tasks are left in; `batch retry` resets them to Pending before
/// the pass so they flow through normally.
fn filter_embeddable_by_task_state(
    task_store: &TaskStore,
    pending: Vec<EmbeddableChunk>,
    override_budget: bool,
    control: &ControlHandle,
) -> Result<Vec<EmbeddableChunk>> {
    validate_embedding_task_bindings(task_store, &pending)?;
    let tasks = task_store.all().map_err(pipeline_to_kio)?;
    let mut by_ref = BTreeMap::<String, Vec<&TaskDescriptor>>::new();
    for task in tasks
        .iter()
        .filter(|task| task.task_type == TaskType::Embedding)
    {
        by_ref
            .entry(task.output_ref.clone())
            .or_default()
            .push(task);
    }
    Ok(pending
        .into_iter()
        .filter(|chunk| {
            let output_ref = embedding_task_output_ref(chunk);
            match by_ref.get(&output_ref) {
                Some(tasks) => tasks
                    .iter()
                    .filter(|task| task.fallback_reason.as_deref() != Some(RETIRED_NON_LIVE))
                    .all(|task| embeddable_task_state(task, override_budget, control)),
                // No task row yet (should not happen post-enqueue): embed it.
                None => true,
            }
        })
        .collect())
}

/// Whether an embedding task's current state permits embedding its chunk now.
fn embeddable_task_state(
    task: &TaskDescriptor,
    override_budget: bool,
    control: &ControlHandle,
) -> bool {
    match task.status {
        TaskStatus::Paused => match task.fallback_reason.as_deref() {
            // Sticky budget pause (L2 ii): only an explicit override re-includes it.
            Some("budget_exceeded") => override_budget,
            // R21-1: a secrets hold must NEVER be re-driven into the send pipeline by a
            // sendable chunk (e.g. a non-secret content-twin whose JOIN fan-out shares
            // this held task's output_ref) — only `--send-secrets` (which flips the task
            // back to Pending via the exact approval release path may. Defense-in-depth behind
            // the R21-1 JOIN dedup so the hold cannot be bypassed even if a same
            // output_ref collision re-appears from another source.
            Some(SECRETS_TIER_B_HOLD) => false,
            // QA2 (step4b-contract-tests-p3a.md §A): an auth hold is released
            // only by the dedicated revive pre-pass
            // (`reconcile_committed_embedding_tasks`'s `auth_revive_candidate`,
            // gated on `batch resume`'s `allow_auth_revive`) — never by being
            // re-driven directly from this generic sendable-chunk path.
            Some("auth_error") => false,
            // Any other Paused reason is safe to re-drive.
            _ => true,
        },
        // Failed embeddings are owned by `batch retry` (L7): skip unless the
        // backoff has elapsed AND the error is still retryable.
        TaskStatus::Failed => task_retry_due(task, control) && task_retry_allowed(task),
        // QA3 (step4b-contract-tests-p3a.md §A, 04 §876): a rate-limited
        // Pending chunk (`next_retry_at` in the future) must not be re-driven
        // before its backoff elapses — "next_retry_at 未来 … の embedding
        // タスクを持つ chunk は enrichment 対象から除外する". Mirrors the
        // markdownize send-selection loop's own `task_retry_due` guard on its
        // Pending arm. A fresh Pending task (`next_retry_at = None`) is
        // unaffected (`task_retry_due` defaults to due).
        TaskStatus::Pending => task_retry_due(task, control),
        _ => true,
    }
}

/// Embedding identity hash (03 §8.1) keyed on the chunk's `text_hash` so identical
/// content shares one `embeddings` row (content-based reuse).
/// Publish one vector into `objects/embeddings/` — the CAS truth the
/// `embeddings` table and `chunk_vec` are an acceleration layer over (04 §4.3,
/// 03 §8.1).
///
/// R25-6: the object type was declared in the spec and produced by nothing, so
/// `kio repair rebuild-db` snapshotted vectors out of the very database it was
/// about to replace. Losing `sqlite.db` meant buying every vector from the API
/// again. This is the write half of closing that.
///
/// The retained CAS write must succeed before any source or replica row is
/// published. A projection must never be the only copy of a provider result.
fn publish_embedding_object(
    repo: &Repository,
    embedding_hash: &str,
    text_hash: &str,
    vector: &[f32],
    profile: &DeclaredEmbeddingProfile,
    context: Option<&str>,
) -> Result<()> {
    let object = kio_core::cas::EmbeddingObject {
        spec_version: 1,
        target_type: "chunk".to_owned(),
        target_hash: text_hash.to_owned(),
        profile_hash: profile.profile_hash.clone(),
        modality: profile.modality.clone(),
        dimensions: profile.dimensions,
        distance: profile.distance.clone(),
        context: context.map(str::to_owned),
        vector: vector.to_vec(),
    };
    publish_verified_embedding(repo, embedding_hash, &object)
}

/// Image vectors obey the same CAS-before-projection publication contract.
fn publish_image_embedding_object(
    repo: &Repository,
    embedding_hash: &str,
    image_hash: &str,
    vector: &[f32],
    profile: &DeclaredEmbeddingProfile,
) -> Result<()> {
    let object = kio_core::cas::EmbeddingObject {
        spec_version: 1,
        target_type: "image".to_owned(),
        target_hash: image_hash.to_owned(),
        profile_hash: profile.profile_hash.clone(),
        modality: profile.modality.clone(),
        dimensions: profile.dimensions,
        distance: profile.distance.clone(),
        context: None,
        vector: vector.to_vec(),
    };
    publish_verified_embedding(repo, embedding_hash, &object)
}

fn publish_verified_embedding(
    repo: &Repository,
    expected_hash: &str,
    object: &kio_core::cas::EmbeddingObject,
) -> Result<()> {
    let actual_hash = repo.object_store().write_embedding(object)?;
    if actual_hash != expected_hash {
        return Err(KioError::new(
            "KIO-E-STORE-CORRUPT-001",
            "embedding CAS identity differs from its requested projection key",
            json!({ "expected_hash":expected_hash, "actual_hash":actual_hash }),
            ExitCode::PermanentFailure,
        ));
    }
    Ok(())
}

fn chunk_embedding_hash(
    chunk: &RetainedEmbeddingChunk,
    profile: &DeclaredEmbeddingProfile,
) -> Result<String> {
    // Contextual-embedding addendum (07 §5.3, 2026-07-24): fold the chunk's
    // humanized filename context into the content-addressed identity, matching
    // the context prepended to the adapter input in `send_embed_group`. Two
    // chunks with identical bodies but different filenames therefore get
    // distinct `embedding_hash`es (and distinct vectors), never a shared one.
    let context = embedding_store::chunk_embedding_context(&chunk.raw_path);
    embedding_store::embedding_hash(
        EmbeddingTargetType::Chunk,
        &chunk.text_hash,
        profile.dimensions,
        EmbeddingDistance::Cosine,
        EmbeddingModality::Multimodal,
        &profile.profile_hash,
        context.as_deref(),
    )
    .map_err(index_to_kio)
}

/// Published `gemini-embedding-2` STANDARD (sync) text input rate, per token
/// (07 §5.3 の 2026-07-24 訂正: $0.20 / 1M sync, $0.10 / 1M batch). Used when
/// `tools.toml` declares no `[embedding.*.pricing] tokens_in`; the declared
/// table is the normative source when it is present.
const EMBEDDING_SYNC_USD_PER_TOKEN: f64 = 0.000_000_2; // $0.20 / 1M

/// R24: the Batch lane is billed at half the standard rate. This is a
/// MULTIPLIER rather than a second absolute constant because the price itself
/// comes from `tools.toml`, whose `[pricing]` keys are a closed enum
/// (`pages` / `tokens_in` / `tokens_out`, `tool_lock::PRICING_KIND_ENUM`) with
/// no lane dimension — a declaration cannot state both lanes. The declared
/// price is therefore read as the STANDARD rate and the batch rate is
/// derived, so a user who corrects the published price keeps the 2:1 ratio the
/// provider actually bills.
///
/// The ratio is 2:1 for BOTH priced roles — OCR is $4 vs $2 per 1,000 pages and
/// embedding is $0.20 vs $0.10 per 1M tokens — so one multiplier covers them.
const BATCH_RATE_MULTIPLIER: f64 = 0.5;

/// A role's declared `[pricing]` table with `lane` applied to every entry.
///
/// This is the single point where a declared price becomes a charge, and it
/// exists because there was no such point: both settlement sites read
/// the declared pricing RAW. That was accidentally right for
/// markdownize — its declaration happened to hold the batch rate — which made
/// a `--realtime` OCR run settle at half what the provider bills. An
/// under-record is the direction a budget cap cannot protect against, since
/// the cap only ever sees the too-small number.
///
/// Reading every declaration as the standard rate makes the two roles obey one
/// rule (07 §5.3) instead of opposite conventions that have to be remembered
/// per role.
fn lane_adjusted_pricing(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    role: &str,
    lane: PreferredRequestKind,
) -> std::collections::BTreeMap<String, f64> {
    adapter_settings
        .declared_pricing(role)
        .into_iter()
        .map(|(kind, standard)| (kind, lane_rate(standard, lane)))
        .collect()
}

/// The online send lane resolved for THIS invocation.
///
/// 2026-07-24 ユーザー裁定: OCR と embedding は**必ず同一レーン**で送る — 片方
/// だけ Batch という組み合わせは作らない。したがってレーンは adapter ごとでは
/// なく invocation ごとに 1 つ決まり、この 1 セルがそれを持つ。
///
/// 解決順は network opt-in (07 §3) と同じ形: **CLI → scope config → user config
/// → 既定 (Batch)**。既定を Batch にするのは半額 (OCR $2/1,000 pages・embedding
/// $0.10/1M tokens) だからで、`--realtime` は倍額を承知の明示 opt-in である。
/// これは 2026-07-23 の「OCR 課金は Batch レーンのみ」裁定を明示的に上書きする。
///
/// 0 = 未解決 (= 既定の Batch として読む), 1 = Batch, 2 = Realtime.
static INVOCATION_LANE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

const LANE_CELL_BATCH: u8 = 1;
const LANE_CELL_REALTIME: u8 = 2;

/// The `--realtime` / `--batch` pair as parsed off one command.
#[derive(Debug, Clone, Copy, Default)]
struct LaneOverride {
    realtime: bool,
    batch: bool,
}

impl LaneOverride {
    fn new(realtime: bool, batch: bool) -> Self {
        Self { realtime, batch }
    }
}

/// Resolve and record the invocation lane. Call once per command, before any
/// online send is planned or priced — both the OCR lane selection and the
/// reservation estimates read the result.
///
/// `scope_config` is the scope's `.kio/config.toml` when the command has a
/// repository open; `None` skips straight to the user config.
fn resolve_invocation_lane(
    over: LaneOverride,
    scope_config: Option<&Path>,
    control: &ControlHandle,
) -> Result<()> {
    if over.realtime && over.batch {
        return Err(KioError::invalid_usage(
            "--realtime and --batch are mutually exclusive",
        ));
    }
    let lane = if over.realtime {
        LANE_CELL_REALTIME
    } else if over.batch {
        LANE_CELL_BATCH
    } else {
        let configured = scope_config
            .and_then(kio_core::scope::read_adapter_lane)
            .or_else(|| kio_core::scope::read_adapter_lane(&user_config_toml_path(control)));
        match configured.as_deref() {
            Some("realtime") => LANE_CELL_REALTIME,
            _ => LANE_CELL_BATCH,
        }
    };
    INVOCATION_LANE.store(lane, std::sync::atomic::Ordering::SeqCst);
    Ok(())
}

/// The lane BOTH online adapters send on this invocation. There is no
/// per-adapter answer by ruling — see [`INVOCATION_LANE`].
fn effective_invocation_lane() -> PreferredRequestKind {
    if INVOCATION_LANE.load(std::sync::atomic::Ordering::SeqCst) == LANE_CELL_REALTIME {
        PreferredRequestKind::Sync
    } else {
        PreferredRequestKind::Batch
    }
}

#[must_use]
fn realtime_lane_requested() -> bool {
    matches!(effective_invocation_lane(), PreferredRequestKind::Sync)
}

/// The lane the SYNCHRONOUS enrichment path sends on — always `Sync`.
///
/// R24 (3 系統一致) corrected this comment, which still claimed the Batch driver
/// "is still to land". It has landed: [`effective_invocation_lane`] chooses the
/// lane, and when it selects Batch the work goes to
/// [`submit_embedding_batch_jobs`] and never reaches the code that calls this.
///
/// So the callers of this function are, by construction, the ones already on the
/// synchronous lane — either because the invocation asked for `--realtime` or
/// because the batch lane was unavailable and the send fell back. Pricing them
/// at the sync rate is therefore correct, not a placeholder — and since I12 it
/// prices the SETTLEMENT too, not just the reservation, because
/// `embedding_billed_from_usage` resolves the measured tokens through this same
/// lane.
fn active_embedding_send_lane() -> PreferredRequestKind {
    PreferredRequestKind::Sync
}

/// The query lane is ALWAYS synchronous: a search cannot wait out a batch
/// job's turnaround (up to 24 h), so a query embedding is priced at the sync
/// rate regardless of what the enrichment path does.
fn query_embedding_send_lane() -> PreferredRequestKind {
    PreferredRequestKind::Sync
}

/// The per-token rate for `lane`.
///
/// R24 (5 系統一致): this used to `return declared` BEFORE looking at `lane`,
/// so declaring `tokens_in` in `tools.toml` — which the spec calls the
/// normative source, and which the dogfood config does declare — silently
/// collapsed the two lanes to one price. At the time the reservation estimate
/// was also the settled charge, so that recorded every Batch job at the sync
/// rate: a 2x over-record that consumes the budget cap twice as fast as the
/// provider actually bills. Settlement now prices measured tokens (I4/I12) —
/// through this same function — so a wrong lane here corrupts BOTH sides at
/// once rather than only the reservation.
fn embedding_usd_per_token(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    lane: PreferredRequestKind,
) -> f64 {
    let standard = adapter_settings
        .declared_pricing("embedding")
        .get("tokens_in")
        .copied()
        .unwrap_or(EMBEDDING_SYNC_USD_PER_TOKEN);
    lane_rate(standard, lane)
}

/// The lane split applied to a STANDARD per-token rate, whatever its source.
/// Split out from [`embedding_usd_per_token`] so the declared-pricing path is
/// testable: `register_declared_pricing` is a process-global `OnceLock`, so a
/// unit test cannot install a `tools.toml` price and observe the result.
fn lane_rate(standard: f64, lane: PreferredRequestKind) -> f64 {
    match lane {
        PreferredRequestKind::Batch => standard * BATCH_RATE_MULTIPLIER,
        PreferredRequestKind::Sync => standard,
    }
}

/// Rough token count for `text`.
///
/// The previous `chars / 4` assumed Latin script throughout and undercounts
/// CJK badly — a Japanese chunk is closer to one token per character, so a
/// mixed corpus was estimated (and, because `EmbeddingResponse.usage` is
/// `None`, *charged*) well under what it costs. Count CJK scalars as one token
/// each and the rest at the ~4-chars/token Latin ratio.
fn estimate_embedding_tokens(text: &str) -> f64 {
    let mut cjk = 0u64;
    let mut other = 0u64;
    for ch in text.chars() {
        if is_cjk_scalar(ch) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk as f64 + other as f64 / 4.0
}

/// Scalars that tokenize at roughly one token each: Han, kana, Hangul, and the
/// CJK punctuation/fullwidth blocks that sit between them.
fn is_cjk_scalar(ch: char) -> bool {
    matches!(ch as u32,
        0x3000..=0x30FF      // CJK symbols and punctuation, hiragana, katakana
        | 0x3130..=0x318F    // Hangul compatibility jamo
        | 0x3400..=0x4DBF    // CJK unified ideographs extension A
        | 0x4E00..=0x9FFF    // CJK unified ideographs
        | 0xAC00..=0xD7AF    // Hangul syllables
        | 0xF900..=0xFAFF    // CJK compatibility ideographs
        | 0xFF00..=0xFFEF    // Halfwidth and fullwidth forms
        | 0x20000..=0x2FA1F  // CJK extensions B-F and compatibility supplement
    )
}

/// Cost estimate for embedding `text` on `lane`.
///
/// This is the RESERVATION, and — per I12's fix — only the settlement fallback
/// for a send whose response carried no usable usage report. It used to be the
/// settled charge outright, on the since-falsified reasoning that the endpoint
/// returns no token count. It still has to be right: I10 measured this
/// estimator against 33 real jobs and found -32%..+52% error with 24% of jobs
/// UNDER-estimated, and the budget cap gates on the reservation, not on the
/// settlement.
fn estimate_embedding_cost(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    text: &str,
    lane: PreferredRequestKind,
) -> f64 {
    estimate_embedding_tokens(text) * embedding_usd_per_token(adapter_settings, lane)
}

/// Price one embedding send from the usage the adapter reported, degrading to
/// `fallback_estimated_usd` (the reservation) whenever that report is absent or
/// fails to cover every kind the adapter declares it bills for.
///
/// I12: one function for all three send sites — Batch collect, sync query, sync
/// chunk — because pricing one adapter in three places is exactly how the bug
/// happened. The Batch site learned to read `usage` while the two sync sites
/// went on asserting the endpoint had none to give, and nothing in the type
/// system objected. A new send site now has to pick a lane and a fallback, not
/// invent a pricing rule.
fn embedding_billed_from_usage(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    reported: Option<&kio_adapter::types::AdapterUsage>,
    lane: PreferredRequestKind,
    fallback_estimated_usd: f64,
    control: &ControlHandle,
    session: &kio_core::scope::LockSession,
) -> BilledAmount {
    let (billed, uncovered) = resolve_billing_from_reported_usage(
        reported,
        // The embedding adapter declares exactly one billable kind
        // (`gemini_embedding`'s profile: `TokensIn`). Its module is private, so
        // the set is restated rather than fabricated from an `AdapterProfile`
        // these call sites do not have.
        &std::collections::BTreeSet::from(["tokens_in".to_owned()]),
        // The SAME rate resolution the reservation used, fallback included.
        // Pricing the two sides from different sources is how a reservation and
        // its settlement drift apart.
        &std::collections::BTreeMap::from([(
            "tokens_in".to_owned(),
            embedding_usd_per_token(adapter_settings, lane),
        )]),
        fallback_estimated_usd,
    );
    warn_uncovered_pricing_kinds_once(EMBEDDING_ADAPTER_KIND, &uncovered, control, session);
    billed
}

/// Retained current-config chunks that have no usable current-profile vector.
fn retained_chunks_without_embedding(
    conn: &Connection,
    candidates: Vec<EmbeddableChunk>,
    profile: &DeclaredEmbeddingProfile,
) -> Result<Vec<EmbeddableChunk>> {
    let mut pending = Vec::new();
    for chunk in candidates {
        let embedding_hash = chunk_embedding_hash(&chunk, profile)?;
        let has_current_profile = embedding_store::content_vector(conn, &embedding_hash)
            .map_err(index_to_kio)?
            .is_some();
        if has_current_profile {
            continue;
        }
        pending.push(chunk);
    }
    Ok(pending)
}

/// Embed typed-owned image objects cited by live chunks into `image_vec`
/// (04 §4.3).
///
/// # Where the work list comes from
///
/// The chunk body supplies the citation relation, while the authenticated
/// immutable normalized unit supplies the ownership relation. Both are
/// required: a free-form `kio://` URI cannot select same-scope CAS bytes.
///
/// # Why it is gated
///
/// An adapter that declares `modality: "multimodal"` has said something about
/// the vector SPACE, not about what it can be handed. The adopted online
/// adapter declares multimodal and reads only `EmbeddingItem::text`, so giving
/// it an image item embeds the empty string and stores a vector that is wrong
/// without being detectably wrong. `image_object` is the flag that means "this
/// implementation reads `path`/`mime`".
///
/// # Cost
///
/// A local image-capable adapter has no billable ledger operation, but it is
/// still an authenticated HTTP peer. Its caller passes execution admission:
/// exact device+scope approval for HTTP adapters, or the in-process evaluator.
fn run_image_embedding_enrichment(
    invocation: Invocation<'_>,
    embedding: &EmbeddingExecutionContext<'_>,
    conn: &Connection,
    retained_instances: &[RetainedNormalizedInstance],
    eligible_chunks: &[EmbeddableChunk],
    secrets_approved: bool,
    execution_allowed: bool,
) -> Result<usize> {
    let ctx = &invocation;
    let adapter_settings = invocation.settings;

    if !execution_allowed {
        return Ok(0);
    }
    // No online image lane currently has the required ledger/reservation
    // protocol. Never turn a newly advertised capability into unaccounted
    // external spending merely because the adapter accepts image objects.
    if embedding_is_online(embedding.execution) {
        return Ok(0);
    }
    let adapter_profile = embedding_adapter_profile_with_control(
        adapter_settings,
        embedding.execution,
        invocation.control,
    )
    .map_err(adapter_to_kio)?;
    if !adapter_profile
        .capability_flags
        .iter()
        .any(|flag| flag == kio_adapter::catalog::IMAGE_OBJECT_CAPABILITY)
    {
        return Ok(0);
    }
    let scope_id = embedding.repo.scope_identity()?.scope_id;
    let image_authority = image_authority::retained_image_authority(
        embedding.repo,
        retained_instances,
        embedding.current_policy,
    )?;
    let referenced = referenced_authorized_image_hashes_for_chunks(
        eligible_chunks,
        &scope_id,
        &image_authority,
        secrets_approved,
    );
    if referenced.is_empty() {
        return Ok(0);
    }
    let already = embedding_store::embedded_image_hashes(conn, &embedding.profile.profile_hash)
        .map_err(index_to_kio)?;
    // BTreeSet iteration is ordered, so the send order is a function of the
    // corpus and not of hash-map seed — two runs over one corpus embed in the
    // same order.
    let pending = referenced
        .into_iter()
        .filter(|hash| !already.contains(hash))
        .collect::<Vec<_>>();
    if pending.is_empty() {
        return Ok(0);
    }
    let mut embedded = 0;
    let mut replica_marked_unavailable = false;
    let result = (|| -> Result<()> {
        let store = embedding.repo.object_store();
        // The local adapter deliberately makes one HTTP request per image.
        // Keep the app-side authorization and bounded CAS read at that same
        // granularity: a policy change cannot admit later images, and a large
        // corpus never accumulates all source bytes in memory.
        for image_hash in &pending {
            match store.inspect_content_object(ContentObjectKind::Image, image_hash) {
                Ok(_) => {}
                Err(error) if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => continue,
                Err(error) => return Err(error),
            }
            let bytes = store.read_content_object_bytes(
                ContentObjectKind::Image,
                image_hash,
                u64::try_from(MAX_EMBEDDING_ITEM_BYTES)
                    .map_err(|_| KioError::schema("embedding image cap exceeds u64"))?,
            )?;
            if kio_core::cas::hash_bytes(&bytes) != *image_hash {
                return Err(KioError::schema(
                    "verified image payload hash changed while reading",
                ));
            }
            let mime = verified_image_mime(&bytes)?;
            // OCR-extracted figures and directly ingested images share this
            // send boundary. Magic bytes alone are insufficient: validate the
            // bounded decode before an adapter can process any pixels.
            let _ = crate::image_ingest::validate_source_image_payload(&bytes, mime)?;

            // Re-read policy, exact network grant, and secret consent before
            // every authenticated local HTTP request. A secret alias is
            // conservative: it requires this embedding tool's paired grant
            // even if a public alias names the same immutable image object.
            admit_embedding_transport(
                adapter_settings,
                embedding.repo,
                embedding.execution,
                embedding.profile,
                embedding.current_policy,
                image_authority.requires_secret_approval(image_hash),
            )?;
            let Ok(outcome) = run_embedding_adapter(
                adapter_settings,
                embedding.execution,
                vec![EmbeddingItem {
                    id: image_hash.clone(),
                    content: EmbeddingContent::Image {
                        bytes,
                        mime: mime.to_owned(),
                    },
                }],
                EmbeddingInputType::ImageObject,
                // No reservation was made, so there is no intent token to carry.
                None,
                invocation.control,
            ) else {
                // A failed image embed must not fail the index pass. Chunk
                // enrichment is load-bearing; this image remains pending for
                // the next run while later independent images can progress.
                continue;
            };
            let Some(vector) = outcome.vectors.into_iter().next() else {
                return Err(KioError::schema("image embedding response was empty"));
            };
            if vector.id != *image_hash {
                return Err(KioError::schema(
                    "image embedding response id did not match request",
                ));
            }
            let embedding_hash = embedding_store::embedding_hash(
                EmbeddingTargetType::Image,
                &vector.id,
                embedding.profile.dimensions,
                EmbeddingDistance::Cosine,
                // 04 §4.3 / U11: image rows are `multimodal` like every other row.
                // `EmbeddingModality::Image` would name a second vector space,
                // which 03 §7 does not have.
                EmbeddingModality::Multimodal,
                &embedding.profile.profile_hash,
                None,
            )
            .map_err(index_to_kio)?;
            // CAS first, table second. This keeps a crash recoverable by a
            // later rebuild instead of leaving a SQLite reference to no object.
            if !replica_marked_unavailable {
                // Mark before the first visible source mutation. If a later
                // image or authorization check fails, readers cannot mistake
                // a prior image vector for a complete replica projection.
                mark_replica_unavailable_or_log(ctx, embedding.repo.kio_dir());
                replica_marked_unavailable = true;
            }
            publish_image_embedding_object(
                embedding.repo,
                &embedding_hash,
                &vector.id,
                &vector.vector,
                embedding.profile,
            )?;
            embedding_store::write_image_embedding(
                conn,
                &embedding_hash,
                &vector.id,
                &embedding_store::f32_to_le_bytes(&vector.vector),
                embedding.profile.dimensions,
                &embedding.profile.distance,
                &embedding.profile.modality,
                &embedding.profile.profile_hash,
            )
            .map_err(index_to_kio)?;
            embedded += 1;
        }
        Ok(())
    })();
    if embedded > 0 {
        // `ScopeDelta` only represents chunk-vector additions. Image vectors
        // also need their image-reference relation, so publish a complete
        // projection after rotating the source generation.
        rotate_index_generation_unconditionally(
            embedding.repo.kio_dir(),
            embedding.repo.control(),
        )?;
        write_through_projection_or_log(ctx, embedding.repo.kio_dir());
    }
    result?;
    Ok(embedded)
}

/// Infer only image types whose magic bytes are unambiguous. The adapter must
/// not choose a decoder from a path or caller-supplied MIME.
fn verified_image_mime(bytes: &[u8]) -> Result<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Ok("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Ok("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Ok("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Ok("image/webp")
    } else {
        Err(KioError::schema(
            "image object has unsupported or unverifiable MIME",
        ))
    }
}

/// Every image object hash referenced by a live chunk body and owned by a
/// currently eligible retained unit. Markdown is only a citation relation;
/// immutable typed ownership is what permits a local CAS read or vector row.
fn referenced_authorized_image_hashes_for_chunks(
    chunks: &[impl AsRef<RetainedEmbeddingChunk>],
    scope_id: &str,
    authority: &image_authority::ImageAuthorityMap,
    secrets_approved: bool,
) -> BTreeSet<String> {
    let mut hashes = BTreeSet::new();
    for chunk in chunks
        .iter()
        .map(AsRef::as_ref)
        .filter(|chunk| secrets_approved || !chunk.requires_secret_approval)
    {
        for image in extract_related_images(&chunk.text) {
            if let Some(hash) =
                authorized_local_image_object_hash(scope_id, authority, &image.image_uri)
                && (secrets_approved || !authority.requires_secret_approval(&hash))
            {
                hashes.insert(hash);
            }
        }
    }
    hashes
}

/// Only a URI naming this scope may cause a local image CAS read, vector row,
/// or projection association. Foreign-scope Markdown links remain intact for
/// their explicit resolver, but never alias same-hash bytes in this scope.
fn local_image_object_hash(scope_id: &str, image_uri: &str) -> Option<String> {
    let object = kio_search::object_uri::parse_object_uri(image_uri).ok()?;
    (object.is_image() && object.scope_id() == scope_id).then(|| object.hash().to_owned())
}

/// A same-scope URI is necessary but insufficient for a local image-object
/// operation. Its hash must also occur in a retained unit's typed ownership
/// set and have a currently allowed owner alias.
fn authorized_local_image_object_hash(
    scope_id: &str,
    authority: &image_authority::ImageAuthorityMap,
    image_uri: &str,
) -> Option<String> {
    let hash = local_image_object_hash(scope_id, image_uri)?;
    authority.permits(&hash).then_some(hash)
}

fn validate_embedding_task_bindings(
    task_store: &TaskStore,
    chunks: &[EmbeddableChunk],
) -> Result<()> {
    let by_ref = chunks
        .iter()
        .map(|chunk| (chunk.work_key.output_ref(), chunk))
        .collect::<BTreeMap<_, _>>();
    for task in task_store.all().map_err(pipeline_to_kio)? {
        if task.task_type != TaskType::Embedding {
            continue;
        }
        if let Some(chunk) = by_ref.get(&task.output_ref)
            && (task.input_hash != chunk.text_hash
                || task.changed_unit_keys != [chunk.chunk_id.clone()])
        {
            return Err(KioError::schema(
                "embedding task does not match authenticated contextual work",
            ));
        }
    }
    Ok(())
}

fn embedding_task_output_ref(chunk: &EmbeddableChunk) -> String {
    chunk.work_key.output_ref()
}

/// R12-3: complete embedding tasks stranded `Pending`/`Running` by a crash between
/// the per-batch `chunk_vec` commit and the deferred task-store write-back (R11-5).
/// A chunk that already carries its embedding is "live but not in `pending`" — so
/// `live_chunks_without_embedding` never revisits it and no recovery command
/// (index / batch resume/retry / repair) ever reconciles its task, leaving
/// `index_status` reporting phantom pending enrichment permanently. Mark such tasks
/// `Done` (idempotent, no adapter call, no re-charge — the vector is already stored,
/// so search/data are unaffected; only task accounting + the Agent contract heal).
struct EmbeddingReconcileContext<'a> {
    active_work_refs: &'a BTreeSet<String>,
    /// Exact current sync ledger identity per live chunk. This includes the
    /// contextualized input and avoids reconstructing a ledger selector from
    /// a task's bare text hash.
    active_embedding_hashes_by_work_ref: &'a BTreeMap<String, String>,
    pending: &'a [EmbeddableChunk],
    now: &'a str,
    /// The active embedding profile's `tool_profile_hash` — one of the ledger
    /// `TaskKey`'s 4 fields (04 §5.4 L768: "input_hash = §5.5 のタスク同一性
    /// キーと同じ組"); this reconcile pass needs it to look up/release a task's
    /// ledger row by key.
    profile_hash: &'a str,
    allow_auth_revive: bool,
}

fn reconcile_committed_embedding_tasks(
    repo: &Repository,
    task_store: &TaskStore,
    ledger: Option<&LedgerDb>,
    context: EmbeddingReconcileContext<'_>,
) -> Result<()> {
    let pending_ids: BTreeSet<String> = context
        .pending
        .iter()
        .map(|chunk| chunk.work_key.output_ref())
        .collect();
    let live_ids = context.active_work_refs;
    // R18-1: release the ledger reservation of a NON-LIVE embedding task whose send
    // is now stranded. Once the chunk is edited/deleted (non-live) the task can
    // never be retried, so an open ledger row would eat the embedding
    // per-adapter/device cap for the rest of the month and falsely pause unrelated
    // embeddings — the embedding twin of the markdownize R17-3/R18-2 fix. This
    // in-place pass runs BEFORE the transitions loop below (which then re-reads and
    // skips these now-terminal tasks). Only Pending/Running/Failed STAMPED tasks are
    // eligible: a Done task's stamp is REAL spend (its vector is stored), never a
    // charge to release.
    let reservation_scope_id = repo.scope_identity()?.scope_id;
    #[derive(Clone, Copy)]
    enum ReconcileReservationAction {
        Revive,
        Complete,
        Retire,
    }
    let mut actions = BTreeMap::<String, ReconcileReservationAction>::new();
    for task in task_store.all().map_err(pipeline_to_kio)? {
        if task.task_type != TaskType::Embedding {
            continue;
        }
        // Unbilled local work can reconcile without a central ledger. A
        // retained reservation, however, cannot be discarded until its ledger
        // is available to settle it conservatively.
        if ledger.is_none() && task.reservation_claim().is_some() {
            continue;
        }
        let budget_paused = task.status == TaskStatus::Paused
            && task.fallback_reason.as_deref() == Some("budget_exceeded");
        // QA2 (step4b-contract-tests-p3a.md §A): an auth failure now lands
        // Paused (never Failed) — admit it here the SAME way `budget_paused`
        // is admitted, otherwise a Paused(auth) task on a non-live chunk
        // strands its open reservation forever (the R18-1/R22-3 bug class).
        let auth_paused = task.status == TaskStatus::Paused
            && retry_kind_from_reason(task.fallback_reason.as_deref()) == RetryErrorKind::AuthError;
        if !budget_paused
            && !auth_paused
            && !matches!(
                task.status,
                TaskStatus::Pending | TaskStatus::Running | TaskStatus::Failed
            )
        {
            continue;
        }
        // QA2: Paused-based now (was Failed-based) — an auth failure never
        // lands Failed anymore, so no Failed-auth compatibility path is needed
        // (再 init 方針).
        let auth_revive_candidate = context.allow_auth_revive && auth_paused;
        let Some(work_key) = EmbeddingWorkKey::parse(&task.output_ref) else {
            continue;
        };
        let chunk_id = task.output_ref.as_str();
        let live = live_ids.contains(chunk_id);
        let live_embedded = live && !pending_ids.contains(chunk_id);
        let materialized_completion =
            live_embedded && task_can_complete_from_materialized_output(&task);
        if task.reservation_claim().is_none()
            && !auth_revive_candidate
            && !budget_paused
            && !materialized_completion
        {
            continue;
        }
        let action = if live && !live_embedded {
            if auth_revive_candidate {
                ReconcileReservationAction::Revive
            } else {
                continue;
            }
        } else if live_embedded {
            ReconcileReservationAction::Complete
        } else {
            ReconcileReservationAction::Retire
        };
        let key = match task.reservation_id.as_deref() {
            // A task stamped by an earlier send may name a contextualized
            // input that no longer exists at this path. Its intent token is
            // the durable selector for that old attempt; never substitute the
            // current path's hash (or the bare task text hash) for it.
            Some(intent_token) => match resolve_abandon_selector(
                ledger.expect("a stamped reservation requires its ledger"),
                &AbandonSelector::IntentToken(intent_token.to_owned()),
            )
            .map_err(pipeline_to_kio)?
            {
                AbandonResolution::Found(key) => Some(key),
                // The sync terminal transaction clears the row's intent token.
                // That proves there is no longer an open reservation to
                // release, but must not strand a vector-complete task after a
                // crash before its deferred Done transition.
                AbandonResolution::NotFound => None,
                AbandonResolution::Ambiguous => continue,
            },
            None => {
                let Some(input_hash) = context.active_embedding_hashes_by_work_ref.get(chunk_id)
                else {
                    // A non-live, unstamped task has no durable contextualized
                    // identity. Leave it for explicit stale recovery rather
                    // than guessing from the bare task text hash.
                    continue;
                };
                Some(task_ledger_key(
                    &reservation_scope_id,
                    EMBEDDING_ADAPTER_KIND,
                    input_hash,
                    context.profile_hash,
                ))
            }
        };
        // `tasks.jsonl` is local portable metadata; an intent token in it is
        // only a lookup hint. It must never expand this scope's authority to
        // mutate a row belonging to another scope or adapter.
        if let Some(key) = key {
            if key.scope_id != reservation_scope_id
                || key.adapter_kind != EMBEDDING_ADAPTER_KIND
                || key.input_hash != work_key.embedding_hash()
            {
                continue;
            }
            if let Some(ledger) = ledger {
                release_task_charge_if_open(ledger, &key)?;
            }
        }
        actions.insert(task.task_id.clone(), action);
    }
    task_store
        .update_matching(|task| {
            let Some(action) = actions.get(&task.task_id).copied() else {
                return false;
            };
            task.clear_reservation();
            match action {
                ReconcileReservationAction::Revive => {
                    task.status = TaskStatus::Pending;
                    task.fallback_reason = None;
                    task.hold_reason = None;
                    task.attempts = 0;
                    task.next_retry_at = None;
                    task.heartbeat_at = None;
                }
                ReconcileReservationAction::Complete => {
                    task.status = TaskStatus::Done;
                    task.fallback_reason = Some("embedding_adapter_done".to_owned());
                    task.next_retry_at = None;
                    task.heartbeat_at = None;
                }
                ReconcileReservationAction::Retire => retire_online_task(task),
            }
            true
        })
        .map_err(pipeline_to_kio)?;
    let mut transitions: BTreeMap<String, EmbeddingTransition> = BTreeMap::new();
    for task in task_store.all().map_err(pipeline_to_kio)? {
        if task.task_type != TaskType::Embedding {
            continue;
        }
        // Unbilled local work can reconcile without a central ledger. A
        // retained reservation, however, cannot be discarded until its ledger
        // is available to settle it conservatively.
        if ledger.is_none() && task.reservation_claim().is_some() {
            continue;
        }
        // R22-3: `Paused` (a `secrets_tier_b_hold`, or a sticky `budget_exceeded`) was the
        // ONE status this non-live sweep never visited, so a held chunk that was later
        // edited or deleted left its task Paused forever — `compute_index_status` counts it
        // as pending enrichment, so editing a Tier B file N times accreted N orphan holds
        // and monotonically decayed `enriched_ratio`. Admit Paused here; the `paused` guard
        // below keeps it out of the Done arm (a hold was never sent, so it must never be
        // reported as embedded).
        let paused = matches!(task.status, TaskStatus::Paused);
        if !matches!(
            task.status,
            TaskStatus::Pending | TaskStatus::Running | TaskStatus::Paused
        ) {
            continue;
        }
        let Some(_work_key) = EmbeddingWorkKey::parse(&task.output_ref) else {
            continue;
        };
        let chunk_id = task.output_ref.as_str();
        // A genuinely un-embedded LIVE chunk stays pending (real outstanding work).
        if pending_ids.contains(chunk_id) {
            continue;
        }
        // R15-7: the chunk is NOT live at HEAD — the file was deleted or re-chunked
        // (a new gen), so this task's chunk no longer exists. It can never be driven to
        // completion (`live_chunks_without_embedding` never revisits it), so leaving it
        // Pending/Running permanently pollutes `index_status`/`status.tasks` with phantom
        // pending enrichment. Terminalize it — but REVERSIBLY (R19-3 `retired_non_live`):
        // no adapter call, no re-charge; and if the exact chunk_id later reappears
        // (git revert / restore), the enqueue guard re-creates a fresh task instead of
        // this being a permanent silent hole in vector search.
        if !live_ids.contains(chunk_id) {
            transitions.insert(
                task.output_ref.clone(),
                embedding_retired_non_live_transition(),
            );
            continue;
        }
        // R22-3: a LIVE Paused task keeps its pause. Reaching here means the chunk is live
        // and already has a vector (its content-hash twin was embedded), but a held chunk's
        // text was never sent — marking it `embedding_adapter_done` would both fake the
        // audit trail and let the exact approval release path find nothing to release.
        if paused {
            continue;
        }
        // Live AND already embedded (live but NOT pending) → complete it (R12-3).
        transitions.insert(task.output_ref.clone(), embedding_done_transition());
    }
    // No fresh reservations to stamp on the reconcile path (it charges nothing).
    apply_embedding_transitions(task_store, &transitions, context.now, &BTreeMap::new())?;
    Ok(())
}

/// N1a: enqueue a held `Embedding` task (Paused `secrets_tier_b_hold`) per Tier B
/// chunk that lacks one, so the hold is visible in `kio status` without ever
/// entering the send pipeline. Idempotent — a chunk that already has an embedding
/// task (held or otherwise) is left untouched.
fn hold_secret_embedding_tasks(
    task_store: &TaskStore,
    repo: &Repository,
    ledger: Option<&LedgerDb>,
    profile: &DeclaredEmbeddingProfile,
    held: &[EmbeddableChunk],
    now: &str,
) -> Result<()> {
    if held.is_empty() {
        return Ok(());
    }
    validate_embedding_task_bindings(task_store, held)?;
    let all_tasks = task_store.all().map_err(pipeline_to_kio)?;
    // R22-2: `existing` used to mean "any non-retired task ⇒ already classified", which
    // silently assumed a chunk's secret classification never changes after its task is
    // created. It does: renaming a plain file INTO a Tier B name (`notes.md` →
    // `credentials_backup.md`) re-partitions the chunk into `held` on the very next index
    // (the partition reads the live `te.path`, R20-1), yet this guard skipped the demotion,
    // so the task stayed Pending/`network_opt_in_required` forever while `quarantine.jsonl`
    // recorded a "hold". Nothing was ever sent (the send set is recomputed from content each
    // pass), but the N1a disclosure contract broke and no recovery command converged.
    // Only an ALREADY-HELD task means "correctly classified"; everything else is demoted.
    let already_held = all_tasks
        .iter()
        .filter(|task| task.task_type == TaskType::Embedding)
        .filter(|task| {
            task.status == TaskStatus::Paused
                && task.fallback_reason.as_deref() == Some(SECRETS_TIER_B_HOLD)
        })
        .map(|task| task.output_ref.clone())
        .collect::<BTreeSet<_>>();
    // A `Done` task's vector is real spend that already exists in `embeddings`; demoting it
    // to a hold would fake outstanding work and strand the stored vector. R20-10 keeps the
    // held chunk out of `chunk_vec`, which is the disclosure that matters here.
    let done = all_tasks
        .iter()
        .filter(|task| task.task_type == TaskType::Embedding)
        .filter(|task| task.status == TaskStatus::Done)
        .map(|task| task.output_ref.clone())
        .collect::<BTreeSet<_>>();
    // R21-7: a task retired because its chunk went non-live must NOT block a fresh HOLD
    // when the content-addressed chunk_id reappears under a Tier B name (git revert /
    // restore of the exact bytes). `enqueue_embedding_tasks` (the sendable sibling) got
    // this R19-3/R20-2 revive; the held branch was left behind, so a restored-under-a-
    // secret-name chunk got no hold task — invisible in `index_status` (R20-7 excludes
    // retired_non_live) and re-embeddable only via `--send-secrets`.
    let revivable = all_tasks
        .iter()
        .filter(|task| task.task_type == TaskType::Embedding)
        .filter(|task| task.fallback_reason.as_deref() == Some(RETIRED_NON_LIVE))
        .map(|task| task.output_ref.clone())
        .collect::<BTreeSet<_>>();
    let existing_nonretired = all_tasks
        .iter()
        .filter(|task| task.task_type == TaskType::Embedding)
        .filter(|task| task.fallback_reason.as_deref() != Some(RETIRED_NON_LIVE))
        .map(|task| task.output_ref.clone())
        .collect::<BTreeSet<_>>();
    // A secret hold is a policy overlay, not permission to erase a terminal retry
    // decision. Preserve non-retryable and exhausted failures in place so a
    // hold/unhold cycle cannot turn them back into fresh sendable work.
    let terminal_failed = all_tasks
        .iter()
        .filter(|task| task.task_type == TaskType::Embedding)
        .filter(|task| {
            task.status == TaskStatus::Failed
                && task.fallback_reason.as_deref() != Some(RETIRED_NON_LIVE)
                && !task_retry_allowed(task)
        })
        .map(|task| task.output_ref.clone())
        .collect::<BTreeSet<_>>();
    // Live, non-retired, not-yet-held, not Done → demote in place to a Paused hold.
    let demotable = all_tasks
        .iter()
        .filter(|task| task.task_type == TaskType::Embedding)
        .filter(|task| {
            task_can_enter_secret_hold(task)
                && !already_held.contains(&task.output_ref)
                && !done.contains(&task.output_ref)
                && !terminal_failed.contains(&task.output_ref)
                && task.fallback_reason.as_deref() != Some(RETIRED_NON_LIVE)
        })
        .map(|task| task.output_ref.clone())
        .collect::<BTreeSet<_>>();
    let mut to_revive: BTreeSet<String> = BTreeSet::new();
    // Demoted holds carry the chunk's CURRENT (secret) path, so `kio status` names the file
    // that is actually being held rather than whatever it was called when first indexed.
    let mut to_demote: BTreeMap<String, String> = BTreeMap::new();
    for chunk in held {
        let output_ref = embedding_task_output_ref(chunk);
        if already_held.contains(&output_ref) || done.contains(&output_ref) {
            continue;
        }
        if revivable.contains(&output_ref) {
            to_revive.insert(output_ref);
            continue;
        }
        if terminal_failed.contains(&output_ref) {
            continue;
        }
        if demotable.contains(&output_ref) {
            to_demote.insert(output_ref, chunk.raw_path.clone());
            continue;
        }
        if existing_nonretired.contains(&output_ref) {
            continue;
        }
        let task = TaskDescriptor {
            task_id: format!("task_{}", new_ulid(repo.canonical_root())),
            task_type: TaskType::Embedding,
            mode: None,
            input_path: chunk.raw_path.clone(),
            input_hash: chunk.text_hash.clone(),
            previous_raw_hash: None,
            parent_run_id: None,
            changed_unit_keys: vec![chunk.chunk_id.clone()],
            output_ref,
            unit_keys: None,
            status: TaskStatus::Paused,
            attempts: 0,
            next_retry_at: None,
            deadline: None,
            heartbeat_at: None,
            fallback_reason: Some(SECRETS_TIER_B_HOLD.to_owned()),
            created_at: now.to_owned(),
            bbox_annotation_enabled: None,
            // QA1 (step4b-contract-tests-p3a.md §A): the closed hold_reason
            // enum accompanies every Paused task.
            hold_reason: Some(HoldReason::TierBApproval),
            reserved_usd: None,
            reserved_month: None,
            reservation_id: None,
        };
        task_store.append(&task).map_err(pipeline_to_kio)?;
    }
    // Revive retired-non-live tasks in place to a Paused hold, reusing their slot so no
    // duplicate output_ref is created. Their `reserved_*` were cleared at retirement.
    if !to_revive.is_empty() {
        task_store
            .update_matching(|task| {
                if task.task_type == TaskType::Embedding
                    && task.fallback_reason.as_deref() == Some(RETIRED_NON_LIVE)
                    && to_revive.contains(&task.output_ref)
                {
                    task.status = TaskStatus::Paused;
                    task.fallback_reason = Some(SECRETS_TIER_B_HOLD.to_owned());
                    task.hold_reason = Some(HoldReason::TierBApproval);
                    task.attempts = 0;
                    task.next_retry_at = None;
                    task.heartbeat_at = None;
                    task.clear_reservation();
                    true
                } else {
                    false
                }
            })
            .map_err(pipeline_to_kio)?;
    }
    // R22-2: demote an existing non-held task (Pending / Running / Failed / budget-Paused)
    // whose chunk is now classified secret. The hold takes precedence over budget and
    // network reasons (the same precedence `enqueue_online_placeholder_task` applies at
    // creation). A demoted task's still-open ledger reservation now buys nothing — its
    // send is blocked by the hold — so release it (settles `unknown_settled` at the
    // reservation estimate if a row is still open, a no-op otherwise).
    if !to_demote.is_empty() {
        let reservation_scope_id = repo.scope_identity()?.scope_id;
        for task in &all_tasks {
            if task.task_type == TaskType::Embedding && to_demote.contains_key(&task.output_ref) {
                let key = task_ledger_key(
                    &reservation_scope_id,
                    EMBEDDING_ADAPTER_KIND,
                    &task.input_hash,
                    &profile.profile_hash,
                );
                if let Some(ledger) = ledger {
                    release_task_charge_if_open(ledger, &key)?;
                }
            }
        }
        task_store
            .update_matching(|task| {
                if task.task_type != TaskType::Embedding {
                    return false;
                }
                let Some(current_path) = to_demote.get(&task.output_ref) else {
                    return false;
                };
                // Apply the policy hold even without a ledger, while retaining
                // any existing claim for explicit ledger recovery.
                if ledger.is_some() {
                    task.clear_reservation();
                }
                task.status = TaskStatus::Paused;
                task.fallback_reason = Some(SECRETS_TIER_B_HOLD.to_owned());
                task.hold_reason = Some(HoldReason::TierBApproval);
                task.input_path = current_path.clone();
                task.attempts = 0;
                task.next_retry_at = None;
                task.heartbeat_at = None;
                true
            })
            .map_err(pipeline_to_kio)?;
    }
    Ok(())
}

/// Enqueue one Pending `Embedding` task per pending chunk, skipping chunks that
/// already have any embedding task (idempotent re-index).
fn enqueue_embedding_tasks(
    task_store: &TaskStore,
    repo: &Repository,
    pending: &[EmbeddableChunk],
    online: bool,
    now: &str,
) -> Result<()> {
    validate_embedding_task_bindings(task_store, pending)?;
    let all_tasks = task_store.all().map_err(pipeline_to_kio)?;
    // R22-1: a Paused `secrets_tier_b_hold` must not block this enqueue either. Reaching
    // `enqueue_embedding_tasks` means the chunk landed in `sendable`, i.e. NO live path of
    // it is secret (the R21-1 dedup prefers a secret path, so one live secret path would
    // have kept the survivor in `held`) or the scope is `--send-secrets`-approved. The hold
    // is therefore stale: the file was renamed out of a Tier B name, or its secret twin was
    // deleted. Before this, `existing` swallowed the hold and `embeddable_task_state`
    // (R21-1's defense-in-depth) refused to re-drive it, so the chunk fell out of vector
    // search permanently — recoverable only via `--send-secrets`, which persists a
    // scope-wide approval to send every candidate secret. Release it in place instead.
    let hold_revivable = all_tasks
        .iter()
        .filter(|task| task.task_type == TaskType::Embedding)
        .filter(|task| {
            task.status == TaskStatus::Paused
                && task.fallback_reason.as_deref() == Some(SECRETS_TIER_B_HOLD)
        })
        .map(|task| task.output_ref.clone())
        .collect::<BTreeSet<_>>();
    let existing = all_tasks
        .iter()
        .filter(|task| task.task_type == TaskType::Embedding)
        // R19-3: a task retired because its chunk went non-live must NOT block a fresh
        // enqueue when the content-addressed chunk_id reappears (git revert / restore of
        // the exact bytes). Excluding it here lets a genuinely-live-again chunk be
        // re-embedded instead of being silently stuck out of vector search forever.
        .filter(|task| task.fallback_reason.as_deref() != Some(RETIRED_NON_LIVE))
        .filter(|task| !hold_revivable.contains(&task.output_ref))
        .map(|task| task.output_ref.clone())
        .collect::<BTreeSet<_>>();
    // R20-2: output_refs of retired-non-live tasks eligible to be REVIVED IN PLACE. R19-3
    // let a reappearing chunk enqueue past its retired task, but appending a NEW task left
    // the old retired one behind — TWO tasks sharing one `output_ref`. Because
    // `apply_embedding_transitions` and `reconcile_committed_embedding_tasks` key on
    // `output_ref` (not `task_id`), a later rate_limit send re-stamped BOTH and reconcile
    // then reclaimed the single phantom TWICE — a silent per-adapter cap under-count
    // (fail-open) that accumulated each revert cycle. Reviving the existing retired task in
    // place keeps exactly one task per output_ref, so no double-stamp / double-reclaim.
    let revivable = all_tasks
        .iter()
        .filter(|task| task.task_type == TaskType::Embedding)
        .filter(|task| task.fallback_reason.as_deref() == Some(RETIRED_NON_LIVE))
        .map(|task| task.output_ref.clone())
        .collect::<BTreeSet<_>>();
    let reason = if online {
        "ready_for_online_adapter"
    } else {
        "network_opt_in_required"
    };
    let mut to_revive: BTreeSet<String> = BTreeSet::new();
    // R22-1: released holds, mapped to the chunk's CURRENT (non-secret) path so the task's
    // `input_path` stops naming the file it was held under.
    let mut to_unhold: BTreeMap<String, String> = BTreeMap::new();
    for chunk in pending {
        let output_ref = embedding_task_output_ref(chunk);
        if existing.contains(&output_ref) {
            continue;
        }
        if hold_revivable.contains(&output_ref) {
            to_unhold.insert(output_ref, chunk.raw_path.clone());
            continue;
        }
        if revivable.contains(&output_ref) {
            to_revive.insert(output_ref);
            continue;
        }
        let task = TaskDescriptor {
            task_id: format!("task_{}", new_ulid(repo.canonical_root())),
            task_type: TaskType::Embedding,
            mode: None,
            input_path: chunk.raw_path.clone(),
            input_hash: chunk.text_hash.clone(),
            previous_raw_hash: None,
            parent_run_id: None,
            changed_unit_keys: vec![chunk.chunk_id.clone()],
            output_ref,
            unit_keys: None,
            status: TaskStatus::Pending,
            attempts: 0,
            next_retry_at: None,
            deadline: None,
            heartbeat_at: None,
            fallback_reason: Some(reason.to_owned()),
            created_at: now.to_owned(),
            bbox_annotation_enabled: None,
            hold_reason: None,
            reserved_usd: None,
            reserved_month: None,
            reservation_id: None,
        };
        task_store.append(&task).map_err(pipeline_to_kio)?;
    }
    // Revive retired-non-live tasks in place (Failed -> Pending), reusing their slot so no
    // duplicate output_ref is created. Their `reserved_*` were cleared at retirement, so
    // the revived task carries no phantom reservation.
    if !to_revive.is_empty() {
        task_store
            .update_matching(|task| {
                if task.task_type == TaskType::Embedding
                    && task.fallback_reason.as_deref() == Some(RETIRED_NON_LIVE)
                    && to_revive.contains(&task.output_ref)
                {
                    task.status = TaskStatus::Pending;
                    task.fallback_reason = Some(reason.to_owned());
                    task.hold_reason = None;
                    task.attempts = 0;
                    task.next_retry_at = None;
                    task.heartbeat_at = None;
                    task.clear_reservation();
                    true
                } else {
                    false
                }
            })
            .map_err(pipeline_to_kio)?;
    }
    // R22-1: release stale secrets holds in place (Paused -> Pending). The hold carried no
    // reservation (`main.rs` hold creation stamps none), so there is nothing to reclaim.
    // `embeddable_task_state`'s `SECRETS_TIER_B_HOLD => false` guard stays untouched: a hold
    // is still never re-driven while it IS a hold — it is cleared here only because the
    // content-addressed partition just proved the chunk has no live secret path.
    if !to_unhold.is_empty() {
        task_store
            .update_matching(|task| {
                if task.task_type == TaskType::Embedding
                    && task.status == TaskStatus::Paused
                    && task.fallback_reason.as_deref() == Some(SECRETS_TIER_B_HOLD)
                {
                    let Some(current_path) = to_unhold.get(&task.output_ref) else {
                        return false;
                    };
                    task.status = TaskStatus::Pending;
                    task.fallback_reason = Some(reason.to_owned());
                    task.hold_reason = None;
                    task.input_path = current_path.clone();
                    task.attempts = 0;
                    task.next_retry_at = None;
                    task.heartbeat_at = None;
                    true
                } else {
                    false
                }
            })
            .map_err(pipeline_to_kio)?;
    }
    Ok(())
}

// R11-5: the per-batch `update_embedding_tasks` / `complete_` / `pause_` /
// `fail_embedding_tasks` helpers were removed — each did a full tasks.jsonl
// all()+replace_all, and calling them once per 32-chunk batch was the O(N²) hang.
// The enrichment loop now accumulates `EmbeddingTransition`s and writes them back
// once via `apply_embedding_transitions`.

fn retry_reason(kind: RetryErrorKind) -> &'static str {
    match kind {
        RetryErrorKind::NetworkError => "network_error",
        RetryErrorKind::RateLimit => "rate_limit",
        RetryErrorKind::AuthError => "auth_error",
        RetryErrorKind::LocalPeerAuth => "local_peer_auth",
        RetryErrorKind::LocalPeerConfig => "local_peer_config",
        RetryErrorKind::QuotaExceeded => "quota_exceeded",
        RetryErrorKind::InvalidInput => "invalid_input",
        RetryErrorKind::ContractViolation => "contract_violation",
        RetryErrorKind::ResultUnknown => RESULT_UNKNOWN_REASON,
        RetryErrorKind::BudgetExceeded => "budget_exceeded",
    }
}

fn retry_kind_from_reason(reason: Option<&str>) -> RetryErrorKind {
    match reason {
        Some("network_error") => RetryErrorKind::NetworkError,
        Some("rate_limit") => RetryErrorKind::RateLimit,
        Some("auth_error") => RetryErrorKind::AuthError,
        Some("local_peer_auth") => RetryErrorKind::LocalPeerAuth,
        Some("local_peer_config") => RetryErrorKind::LocalPeerConfig,
        Some("quota_exceeded") => RetryErrorKind::QuotaExceeded,
        Some("invalid_input") => RetryErrorKind::InvalidInput,
        Some(RESULT_UNKNOWN_REASON) => RetryErrorKind::ResultUnknown,
        // R19-3: a non-live retirement is non-retryable as the old task (like
        // invalid_input), but the enqueue guards treat it as non-blocking so the
        // content-addressed identity can be re-enqueued if it reappears.
        Some(RETIRED_NON_LIVE) => RetryErrorKind::InvalidInput,
        Some("budget_exceeded") => RetryErrorKind::BudgetExceeded,
        _ => RetryErrorKind::ContractViolation,
    }
}

fn task_retry_allowed(task: &TaskDescriptor) -> bool {
    let kind = retry_kind_from_reason(task.fallback_reason.as_deref());
    let policy = retry_policy(kind);
    policy.retryable
        && policy
            .max_attempts
            .map(|max| task.attempts < max)
            .unwrap_or(true)
}

/// R19-2: an online-send failure carrying an F8 reservation (a NON-billable phantom for
/// RateLimit/Quota/Auth, possibly-billed for NetworkError). A retire+reclaim supersede/sweep
/// must target these REGARDLESS of retry-attempt exhaustion: QuotaExceeded exhausts its
/// finite `max_attempts` (unlike RateLimit's unlimited), so gating on `task_retry_allowed`
/// left an exhausted-quota phantom reservation stuck for the month — never reclaimed,
/// starving the per-adapter cap and falsely pausing legitimate tasks. R20-3: AuthError is
/// included for the same reason — it is non-retryable (`max_attempts:0`) so `task_retry_allowed`
/// excluded it, leaving its (non-billable, 401/403) phantom orphaned; the sweep must retire it
/// so `reclaim_entry_for` can cancel the phantom. The reclaim itself stays error-kind-aware
/// inside `reclaim_entry_for` (RateLimit/Quota/Auth reclaim; NetworkError retires but keeps
/// its reservation as it may have billed). A `retired_non_live` task maps to InvalidInput
/// here, so it is excluded (idempotent).
fn is_reservation_bearing_send_failure(task: &TaskDescriptor) -> bool {
    matches!(
        retry_kind_from_reason(task.fallback_reason.as_deref()),
        RetryErrorKind::RateLimit
            | RetryErrorKind::QuotaExceeded
            | RetryErrorKind::NetworkError
            | RetryErrorKind::AuthError
    )
}

fn task_retry_due(task: &TaskDescriptor, control: &ControlHandle) -> bool {
    task.next_retry_at
        .as_deref()
        .map(|retry_at| retry_at <= control.now_utc_seconds().as_str())
        .unwrap_or(true)
}

/// Absolute time a failed task becomes eligible for retry: `now` plus the
/// backoff derived from its `RetryPolicy.backoff` descriptor (Step2c I2).
/// `attempts` is the post-increment attempt count (>= 1). Falls back to `now`
/// when the clock string cannot be parsed.
fn scheduled_retry_at(now: &str, backoff: &str, attempts: u32) -> String {
    let delay = retry_backoff_seconds(backoff, attempts);
    kio_core::scope::parse_utc_seconds(now)
        .map(|secs| kio_core::scope::format_utc_seconds(secs + delay))
        .unwrap_or_else(|| now.to_owned())
}

/// QA3 (step4b-contract-tests-p3a.md §A, 04 §5.3): `next_retry_at` for a
/// rate-limited send. `Some(ms)` honors the provider's `Retry-After` header
/// exactly — rounded up to whole seconds, floored at 1s so a sub-second
/// header value still schedules strictly in the future. `None` (header
/// absent or unparseable) falls back to the synthetic +2s schedule via
/// `scheduled_retry_at`'s `"retry_after"` backoff descriptor — with this
/// wiring in place, that descriptor's only remaining caller is this
/// fallback, so `retry_backoff_seconds`'s "no server header" comment on it
/// is now literally true rather than merely defensive. Falls back to `now`
/// itself when the clock string cannot be parsed (same posture as
/// `scheduled_retry_at`).
fn rate_limit_retry_at(now: &str, retry_after_ms: Option<u64>) -> String {
    let Some(ms) = retry_after_ms else {
        return scheduled_retry_at(now, "retry_after", 1);
    };
    let delay_seconds = i64::try_from(ms.div_ceil(1000).max(1)).unwrap_or(i64::MAX);
    kio_core::scope::parse_utc_seconds(now)
        .map(|secs| kio_core::scope::format_utc_seconds(secs.saturating_add(delay_seconds)))
        .unwrap_or_else(|| now.to_owned())
}

/// Backoff delay (seconds) for a failed task's next retry, derived from the
/// `RetryPolicy.backoff` descriptor in `crates/kio-pipeline/src/task.rs`.
/// The canonical contract intentionally uses deterministic scheduling without
/// jitter (see `docs/04-pipeline.md` §5.3). `attempts` is the post-increment
/// attempt count (>= 1).
fn retry_backoff_seconds(backoff: &str, attempts: u32) -> i64 {
    // exp(base=Ns,cap=Ms,...): min(base * 2^(attempts-1), cap).
    let exponential = |base: i64, cap: i64| {
        let exponent = attempts.saturating_sub(1).min(16);
        base.saturating_mul(1_i64 << exponent).clamp(0, cap)
    };
    if let Some(inner) = backoff
        .strip_prefix("exp(")
        .and_then(|s| s.strip_suffix(')'))
    {
        let base = duration_secs_field(inner, "base").unwrap_or(2);
        let cap = duration_secs_field(inner, "cap").unwrap_or(60);
        return exponential(base, cap);
    }
    if let Some(inner) = backoff
        .strip_prefix("fixed(")
        .and_then(|s| s.strip_suffix(')'))
    {
        return parse_duration_secs(inner).unwrap_or(3_600);
    }
    // "retry_after": the local placeholder has no server `Retry-After` header,
    // so reuse the exponential schedule (base 2s, cap 60s).
    if backoff == "retry_after" {
        return exponential(2, 60);
    }
    0
}

/// Parse a compact duration such as `2s`, `30m`, or `1h` into seconds.
fn parse_duration_secs(text: &str) -> Option<i64> {
    let text = text.trim();
    let split = text.find(|ch: char| !ch.is_ascii_digit())?;
    let (digits, unit) = text.split_at(split);
    let value = digits.parse::<i64>().ok()?;
    let multiplier = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        _ => return None,
    };
    Some(value * multiplier)
}

/// Extract a `key=<duration>` field (e.g. `base=2s`) from a comma-separated
/// backoff descriptor body.
fn duration_secs_field(inner: &str, key: &str) -> Option<i64> {
    inner.split(',').find_map(|part| {
        let rest = part.trim().strip_prefix(key)?.strip_prefix('=')?;
        parse_duration_secs(rest)
    })
}

fn estimate_online_markdownize_cost(size_bytes: u64, bbox_annotation_enabled: bool) -> f64 {
    let unannotated = estimate_local_baseline_cost(size_bytes) * 10.0;
    let annotated = if bbox_annotation_enabled {
        unannotated * 1.25
    } else {
        unannotated
    };
    // The realtime (sync) lane bills exactly double the Batch lane per page
    // ($4 vs $2 per 1,000 pages), so a `--realtime` run must reserve double —
    // otherwise the budget cap guards half the money actually being spent.
    match effective_invocation_lane() {
        PreferredRequestKind::Sync => annotated * 2.0,
        PreferredRequestKind::Batch => annotated,
    }
}

fn media_type_for_cli_path(path: &Path) -> &'static str {
    // R21-4: lowercase the extension so an uppercase-extension text-native file
    // (`README.MD`, `NOTE.TXT`, `MAIN.RS`) is recognized as text/markdown/plain/code and
    // handled locally — not folded to octet-stream and shipped to online OCR (R9-2).
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "md" | "markdown" => "text/markdown",
        "txt" => "text/plain",
        "rs" | "py" | "js" | "ts" | "go" | "java" | "c" | "h" | "cpp" => "text/x-code",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        // R20-6: recognize OOXML office documents by their real MIME so they are treated as
        // non-text-native (routed to online OCR), not folded into octet-stream and given a
        // raw-bytes local passthrough that evidences the ZIP bytes as searchable text.
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => "application/octet-stream",
    }
}

/// `kio status`'s paused count broken down by the closed `hold_reason` enum.
/// A current task record always stamps one matching reason for a paused task,
/// so the three named buckets always appear (even at 0) and there is no
/// compatibility/unknown bucket.
fn paused_tasks_by_hold_reason(tasks: &[TaskDescriptor]) -> Value {
    let mut budget = 0u64;
    let mut auth = 0u64;
    let mut tier_b_approval = 0u64;
    let mut ledger_initialization_required = 0u64;
    for task in tasks {
        if task.status != TaskStatus::Paused {
            continue;
        }
        match task.hold_reason {
            Some(HoldReason::Budget) => budget += 1,
            Some(HoldReason::Auth) => auth += 1,
            Some(HoldReason::TierBApproval) => tier_b_approval += 1,
            Some(HoldReason::LedgerInitializationRequired) => ledger_initialization_required += 1,
            None => unreachable!("TaskStore accepts only stamped paused tasks"),
        }
    }
    json!({
        "budget": budget,
        "auth": auth,
        "tier_b_approval": tier_b_approval,
        "ledger_initialization_required": ledger_initialization_required,
    })
}

/// R9-2: whether `media_type` is text-native (Markdown / plain text / code) — the
/// deterministic Adapter's domain (docs/07 §2.1). Text-native files are
/// markdownized locally and must never enqueue an online Mistral-OCR task: docs/07
/// §5.2 scopes the standard online Adapter to *non*-text-native PDF / DOCX / PPTX /
/// images. Sending a text file to OCR shipped its raw bytes to a third-party API
/// (privacy) and billed ~10x the baseline for work the deterministic pass already
/// did (redundant, and orphaned by F6).
fn is_text_native_media(media_type: &str) -> bool {
    matches!(media_type, "text/markdown" | "text/plain" | "text/x-code")
}

fn budget_status_json(repo: &Repository, ledger: Option<&LedgerReadSnapshot>) -> Result<Value> {
    let caps = read_budget_policy(
        user_config_toml_path(repo.control()),
        repo.kio_dir().join("config.toml"),
    )
    .map_err(pipeline_to_kio)?;
    let now = repo.control().now_utc_seconds();
    let month = utc_month(&now);
    let scope_id = repo.scope_identity()?.scope_id;
    let Some(ledger) = ledger else {
        return Ok(json!({
            "month": month,
            "device_monthly_usd_cap": caps.device_monthly_usd_cap,
            "folder_monthly_usd_cap": caps.folder_monthly_usd_cap,
            "device_spent_usd": Value::Null,
            "folder_spent_usd": Value::Null,
            "device_remaining_usd": Value::Null,
            "folder_remaining_usd": Value::Null,
            "cap_kind": Value::Null,
            "device_per_adapter": caps.device_per_adapter,
            "hard_stop": caps.hard_stop,
            "warn_at_percent": caps.warn_at_percent,
            "warned": false,
            "warning": Value::Null,
            "ledger_status": "uninitialized",
        }));
    };
    let ledger_path = ledger_db_path(repo.control());
    let device_spent = ledger
        .month_total(None, None, &month)
        .map_err(|error| ledger_snapshot_to_kio(&ledger_path.display().to_string(), error))?;
    let folder_spent = ledger
        .month_total(Some(&scope_id), None, &month)
        .map_err(|error| ledger_snapshot_to_kio(&ledger_path.display().to_string(), error))?;
    let device_remaining = caps.device_monthly_usd_cap - device_spent;
    let folder_remaining = caps.folder_monthly_usd_cap.map(|cap| cap - folder_spent);
    let cap_kind = match folder_remaining {
        Some(folder) if folder <= device_remaining => BudgetCapKind::Folder,
        _ => BudgetCapKind::Device,
    };
    // F5: surface the non-blocking `warn_at_percent` warning and the active
    // policy (hard_stop / warn_at_percent) alongside the numbers.
    let warning = budget_warning(&caps, device_spent, folder_spent);
    Ok(json!({
        "month": month,
        "device_monthly_usd_cap": caps.device_monthly_usd_cap,
        "folder_monthly_usd_cap": caps.folder_monthly_usd_cap,
        "device_spent_usd": device_spent,
        "folder_spent_usd": folder_spent,
        "device_remaining_usd": device_remaining,
        "folder_remaining_usd": folder_remaining,
        "cap_kind": match cap_kind {
            BudgetCapKind::Device => "device",
            BudgetCapKind::Folder => "folder",
        },
        "device_per_adapter": caps.device_per_adapter,
        // QA11 (step4b-contract-tests-p3a.md §D): folder per_adapter does not
        // exist (04 §5.4 — device-layer only), so `kio status` must not
        // present a constraint that is not real.
        "hard_stop": caps.hard_stop,
        "warn_at_percent": caps.warn_at_percent,
        "warned": warning.is_some(),
        "warning": warning,
    }))
}

/// F5: the current scope's non-blocking budget warning (or `None`), for embedding
/// in `index` / `batch` result JSON. Reads the caps and this month's ledger totals.
fn scope_budget_warning(repo: &Repository) -> Result<Option<String>> {
    let caps = read_budget_policy(
        user_config_toml_path(repo.control()),
        repo.kio_dir().join("config.toml"),
    )
    .map_err(pipeline_to_kio)?;
    let path = ledger_db_path(repo.control());
    let (month, snapshot) = capture_search_budget_observation(&path, repo.control())?;
    let Some(snapshot) = snapshot else {
        return Ok(None);
    };
    let scope_id = repo.scope_identity()?.scope_id;
    let device_spent = snapshot
        .month_total(None, None, &month)
        .map_err(|error| ledger_snapshot_to_kio(&path.display().to_string(), error))?;
    let folder_spent = snapshot
        .month_total(Some(&scope_id), None, &month)
        .map_err(|error| ledger_snapshot_to_kio(&path.display().to_string(), error))?;
    Ok(budget_warning(&caps, device_spent, folder_spent))
}

fn index_preview_json(root: &Path, preview: &ScanPreview) -> Value {
    let included = preview
        .candidates
        .iter()
        .filter(|candidate| !candidate.ignored)
        .collect::<Vec<_>>();
    let estimated_size_bytes = included
        .iter()
        .map(|candidate| candidate.size_bytes)
        .sum::<u64>();
    let excluded_candidates = preview
        .candidates
        .iter()
        .filter(|candidate| candidate.ignored)
        .map(|candidate| candidate.input_path.clone())
        .collect::<Vec<_>>();
    let sensitive_candidates = preview
        .candidates
        .iter()
        .filter(|candidate| candidate.quarantine_reason.is_some())
        .map(|candidate| {
            json!({
                "path": candidate.input_path,
                "reason": candidate.quarantine_reason,
                "ignored": candidate.ignored,
            })
        })
        .collect::<Vec<_>>();
    let large_files = preview
        .candidates
        .iter()
        .filter(|candidate| candidate.size_bytes >= 10 * 1024 * 1024)
        .map(|candidate| {
            json!({
                "path": candidate.input_path,
                "size_bytes": candidate.size_bytes,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "status": "preview",
        "root": root,
        "scope": preview.scope_id,
        "estimated_file_count": included.len(),
        "estimated_size_bytes": estimated_size_bytes,
        "large_files": large_files,
        "effective_ignore": ".kioignore + built-in secrets Tier A",
        "excluded_candidates": excluded_candidates,
        "sensitive_candidates": sensitive_candidates,
        "network_transmission_policy": {
            "default": "disabled until adapter approve",
            "yes_grants_network": false,
        },
        "adapter_execution_mode": {
            "markdownize": "deterministic_library baseline",
            "embedding": "not generated without online opt-in",
        },
        "estimated_cost": preview.estimated_cost,
        "budget_cap": preview.estimated_cost.as_ref().and_then(|cost| cost.budget_cap_usd),
        "estimated_completion": "baseline completes in this run; online enhancement depends on budget and opt-in",
        "candidates": preview.candidates,
    })
}

#[derive(Debug, Default)]
struct IndexPipelineResult {
    normalize_by_path: BTreeMap<String, PendingNormalizeRef>,
    network_allowed: bool,
    pending_online_tasks: usize,
    paused_tasks: usize,
    normalized_files: usize,
    pending_files: usize,
    failed_files: usize,
    // R12-2: files larger than adapter.policy.max_input_bytes, skipped for adapter
    // processing (input gate, 07 §7.1.2) but still archived.
    skipped_oversized_files: usize,
    // R22-4: binary inputs with no locally-extractable text whose media type is not an
    // OCR-able one (R21-4 skips their online task). Archived, but never enriched — surfaced
    // so `enriched_ratio: 1.0` cannot silently mean "this file does not exist".
    skipped_unrecognized_binary_files: usize,
}

/// Reuse only a HEAD-pinned local OCR instance whose profile is exactly the
/// local OCR profile selected by current configuration for this media type.
fn reusable_current_ocr_normalize(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    head: &BTreeMap<String, (String, NormalizeRef)>,
    path: &str,
    raw_hash: &str,
    media_type: &str,
) -> Result<Option<NormalizeRef>> {
    let Some((head_raw, normalize)) = head.get(path) else {
        return Ok(None);
    };
    if head_raw != raw_hash {
        return Ok(None);
    }
    let Some(execution) = active_local_ocr_execution_with_control(adapter_settings, repo.control())
        .filter(|_| local_ocr_handles_media_type(media_type))
    else {
        return Ok(None);
    };
    let selected = kio_adapter::local_ocr_markdownize::profile_for(execution);
    if normalize.tool_profile_hash != selected.tool_profile_hash {
        return Ok(None);
    }
    // A HEAD pointer is authority, not a best-effort cache. Reuse requires
    // a nonempty immutable closure whose every selected unit is durably Done;
    // a partial or failed local OCR run must remain pending for local drain.
    if !pinned_manifest_is_complete(repo, raw_hash, normalize)? {
        return Ok(None);
    }
    Ok(Some(normalize.clone()))
}

fn online_output_ref(adapter_id: &str) -> String {
    format!("online:{adapter_id}")
}

/// Stage 3: the enrichment task of a local-HTTP markdownize adapter.
///
/// The prefix is load-bearing, not cosmetic. `output_ref` is what the online
/// lane matches on (`targets_standard_online_markdownize`), so a local
/// pipeline's task keyed `online:` would be swept up by the ledger reservation
/// and batch sender. One prefix keeps local execution separate while its exact
/// device+scope approval is enforced at dispatch.
fn offline_output_ref(adapter_id: &str) -> String {
    format!("offline:{adapter_id}")
}

/// The status a freshly-enqueued `offline_api` markdownize task takes.
///
/// The budget branch has nothing to weigh — a local run has no price. Network
/// approval and `--offline` are evaluated at dispatch, where a missing grant
/// leaves this task Pending without constructing the local HTTP client. A Tier A
/// credential still needs `--send-secrets` before it is handed to a local model
/// server, which is a separate process that can log what it is given.
fn offline_markdownize_enqueue_status(secrets_hold: bool) -> (TaskStatus, Option<&'static str>) {
    if secrets_hold {
        (TaskStatus::Paused, Some(SECRETS_TIER_B_HOLD))
    } else {
        (TaskStatus::Pending, Some("ready_for_local_adapter"))
    }
}

fn explicitly_allowed_tier_a_paths(preview: &ScanPreview) -> BTreeSet<String> {
    preview
        .candidates
        .iter()
        .filter(|candidate| {
            !candidate.ignored
                && candidate.quarantine_reason.as_deref() == Some("secrets_tier_a_online_hold")
        })
        .map(|candidate| candidate.input_path.clone())
        .collect()
}

fn online_markdownize_profile() -> AdapterProfile {
    standard_online_markdownize_profile()
}

fn online_markdownize_profile_for(repo: &Repository) -> Result<AdapterProfile> {
    Ok(standard_online_markdownize_profile_with_bbox(
        bbox_annotation_enabled(repo)?,
    ))
}

#[cfg(debug_assertions)]
fn active_markdown_adapter(_repo: &Repository) -> Box<dyn MarkdownizeAdapter> {
    match markdownize_test_mode(_repo.control()) {
        Some(MarkdownizeTestMode::Incremental) => Box::new(TestIncrementalMarkdownizeAdapter {
            reject_incremental: false,
            reject_full: false,
        }),
        Some(MarkdownizeTestMode::RejectIncremental) => {
            Box::new(TestIncrementalMarkdownizeAdapter {
                reject_incremental: true,
                reject_full: false,
            })
        }
        Some(MarkdownizeTestMode::RejectIncrementalAndFull) => {
            Box::new(TestIncrementalMarkdownizeAdapter {
                reject_incremental: true,
                reject_full: true,
            })
        }
        _ => builtin_offline_markdownize_adapter(),
    }
}

#[cfg(not(debug_assertions))]
fn active_markdown_adapter(_repo: &Repository) -> Box<dyn MarkdownizeAdapter> {
    builtin_offline_markdownize_adapter()
}

#[cfg(debug_assertions)]
fn offline_markdownize_from_verified_bytes(
    adapter: &dyn MarkdownizeAdapter,
    request: MarkdownizeRequest,
    verified_raw_bytes: &[u8],
    control: &ControlHandle,
) -> kio_adapter::Result<kio_adapter::types::MarkdownizeResponse> {
    if markdownize_test_mode(control).is_some() {
        return adapter.markdownize(request);
    }
    // 07 §5.1: DOCX/PPTX get an offline baseline too, sourced from the SAME
    // converted-PDF page/slide text `write_prepared_objects` already verified
    // against Prepare's declared `prepared_hash` -- not the raw OOXML zip bytes
    // (which `markdownize_from_bytes`'s generic non-PDF fallback would otherwise
    // lossy-UTF8-decode as unsearchable garbage). This candidate only reaches
    // here with non-empty prepared units, i.e. a converter resolved for it at
    // Prepare time in this same `kio index` pass; absence here is the same rare
    // mid-run race `write_prepared_objects` already tolerates as a well-defined
    // (never crashing) failure rather than a silent corruption stand-in.
    if let Some(unit_kind) = office_unit_kind(&request.media_type) {
        let converted =
            convert_office_to_pdf_with_control(verified_raw_bytes, &request.media_type, control)?;
        return kio_adapter::deterministic::markdownize_converted_office(
            request, &converted, unit_kind,
        );
    }
    kio_adapter::deterministic::markdownize_from_bytes(request, verified_raw_bytes)
}

#[cfg(not(debug_assertions))]
fn offline_markdownize_from_verified_bytes(
    adapter: &dyn MarkdownizeAdapter,
    request: MarkdownizeRequest,
    verified_raw_bytes: &[u8],
    control: &ControlHandle,
) -> kio_adapter::Result<kio_adapter::types::MarkdownizeResponse> {
    let _ = adapter;
    if let Some(unit_kind) = office_unit_kind(&request.media_type) {
        let converted =
            convert_office_to_pdf_with_control(verified_raw_bytes, &request.media_type, control)?;
        return kio_adapter::deterministic::markdownize_converted_office(
            request, &converted, unit_kind,
        );
    }
    kio_adapter::deterministic::markdownize_from_bytes(request, verified_raw_bytes)
}

#[cfg(debug_assertions)]
#[derive(Clone, Copy)]
enum MarkdownizeTestMode {
    Incremental,
    RejectIncremental,
    RejectIncrementalAndFull,
}

#[cfg(debug_assertions)]
fn markdownize_test_mode(control: &ControlHandle) -> Option<MarkdownizeTestMode> {
    match control.debug().cli.markdownize_adapter.known() {
        Some(MarkdownizeAdapterMode::Incremental) => Some(MarkdownizeTestMode::Incremental),
        Some(MarkdownizeAdapterMode::RejectIncremental) => {
            Some(MarkdownizeTestMode::RejectIncremental)
        }
        Some(MarkdownizeAdapterMode::RejectIncrementalAndFull) => {
            Some(MarkdownizeTestMode::RejectIncrementalAndFull)
        }
        None => None,
    }
}

/// 07-adapter-spec.md §5.1: which `UnitKind` an Office media type's
/// converted-PDF units use -- DOCX enumerates `page:N` (the converted PDF's
/// pages themselves), PPTX enumerates `slide:N` (1 converted-PDF page = 1
/// slide). `None` for every other media type, including XLSX (the sheet
/// conversion mechanism is explicitly out of this addendum's scope, 07 §5.1
/// closing sentence).
fn office_unit_kind(media_type: &str) -> Option<UnitKind> {
    match media_type {
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => {
            Some(UnitKind::Page)
        }
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => {
            Some(UnitKind::Slide)
        }
        _ => None,
    }
}

#[cfg(debug_assertions)]
#[derive(Debug, Clone)]
struct TestIncrementalMarkdownizeAdapter {
    reject_incremental: bool,
    reject_full: bool,
}

#[cfg(debug_assertions)]
impl MarkdownizeAdapter for TestIncrementalMarkdownizeAdapter {
    fn profile(&self) -> AdapterProfile {
        let profile = json!({
            "adapter_kind": "markdownize",
            "adapter_role": "text",
            "model_or_tool_family": "kio-test-incremental",
            "model_version_pin": "1.0.0",
            "output_schema": "kio-markdown-v1",
            "runtime_kind": "local",
            "spec_version": 1
        });
        AdapterProfile {
            adapter_kind: AdapterKind::Markdownize,
            adapter_id: "test_incremental_markdownize".to_owned(),
            execution_mode: ExecutionMode::DeterministicLibrary,
            tool_profile_hash: tool_profile_hash(&profile)
                .expect("built-in test profile should hash"),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            capability_flags: vec!["incremental_update".to_owned()],
            allow_network: false,
            // QA18: this test-seam adapter is local/offline, same posture as
            // the built-in deterministic adapter.
            billable_kinds: Vec::new(),
            reject_billing: Some(kio_adapter::types::BillingDeclaration::Nonbillable),
            // QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880): local/
            // offline test seam, never a sync provider call.
            provider_idempotency: kio_adapter::types::ProviderIdempotency::NotProvided,
        }
    }

    fn markdownize(
        &self,
        request: MarkdownizeRequest,
    ) -> kio_adapter::Result<kio_adapter::types::MarkdownizeResponse> {
        let hints = request.prepared_unit_hint.clone().unwrap_or_default();
        if request.mode == AdapterMarkdownizeMode::Incremental {
            let incremental =
                request
                    .hints
                    .clone()
                    .unwrap_or(kio_adapter::types::IncrementalHints {
                        changed_unit_keys: hints.iter().map(|hint| hint.unit_key.clone()).collect(),
                        added_unit_keys: Vec::new(),
                        removed_unit_keys: Vec::new(),
                        page_fingerprints: BTreeMap::new(),
                    });
            let changed = incremental.changed_unit_keys;
            let added = incremental.added_unit_keys;
            let unchanged = hints
                .iter()
                .filter(|hint| !changed.contains(&hint.unit_key) && !added.contains(&hint.unit_key))
                .map(|hint| hint.unit_key.clone())
                .collect::<Vec<_>>();
            return Ok(kio_adapter::types::MarkdownizeResponse {
                mode_used: AdapterMarkdownizeMode::Incremental,
                updated_units: hints
                    .iter()
                    .filter(|hint| changed.contains(&hint.unit_key))
                    .map(|hint| test_markdown_unit(hint, "incremental"))
                    .collect(),
                unchanged_unit_keys: if self.reject_incremental {
                    Vec::new()
                } else {
                    unchanged
                },
                added_units: hints
                    .iter()
                    .filter(|hint| added.contains(&hint.unit_key))
                    .map(|hint| test_markdown_unit(hint, "incremental-added"))
                    .collect(),
                removed_unit_keys: incremental.removed_unit_keys,
                failed_units: Vec::new(),
                fallback_to_full: false,
                reason: None,
                // QA17: this is a local test-seam adapter, never a billable
                // online call.
                usage: None,
            });
        }
        Ok(kio_adapter::types::MarkdownizeResponse {
            mode_used: AdapterMarkdownizeMode::Full,
            updated_units: if self.reject_full {
                Vec::new()
            } else {
                hints
                    .iter()
                    .map(|hint| test_markdown_unit(hint, "full"))
                    .collect()
            },
            unchanged_unit_keys: Vec::new(),
            added_units: Vec::new(),
            removed_unit_keys: Vec::new(),
            failed_units: Vec::new(),
            fallback_to_full: false,
            reason: None,
            // QA17: see above — never a billable online call.
            usage: None,
        })
    }
}

#[cfg(debug_assertions)]
fn test_markdown_unit(hint: &PreparedUnitHint, prefix: &str) -> MarkdownUnit {
    MarkdownUnit {
        unit_key: hint.unit_key.clone(),
        unit_type: hint.unit_kind,
        markdown: format!("{prefix} {}\n", hint.unit_key),
        owned_image_hashes: BTreeSet::new(),
        metadata: BTreeMap::new(),
    }
}

fn run_index_pipeline(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    preview: &ScanPreview,
    args: &IndexArgs,
) -> Result<IndexPipelineResult> {
    let task_store = TaskStore::new_with_control(repo.kio_dir(), repo.control().clone());
    // Q3: index holds the folder store lock end-to-end, so any Running online task
    // is an orphan from a crashed run. Reclaim it to Pending before the enqueue /
    // dedup pass so it is re-counted as pending_online_tasks (and later re-sent by
    // `batch resume`) rather than being stuck forever in the Running absorbing
    // state — invisible to the pending counter and unrecoverable by any command.
    reclaim_orphaned_running_tasks(&task_store)?;
    let now = repo.control().now_utc_seconds();
    let scope_id = preview.scope_id.clone();
    let prepare_profile_hash = builtin_prepare_profile().tool_profile_hash;
    let markdown_adapter = active_markdown_adapter(repo);
    let markdown_profile = markdown_adapter.profile();
    let markdown_profile_hash = markdown_profile.tool_profile_hash.clone();
    let network_allowed = network_allowed(adapter_settings, repo, args)?;
    let ledger = if args.offline {
        None
    } else {
        open_optional_ledger_db(repo.control())?
    };
    let budget_caps = read_budget_policy(
        user_config_toml_path(repo.control()),
        repo.kio_dir().join("config.toml"),
    )
    .map_err(pipeline_to_kio)?;

    // An offline invocation intentionally has no mutable ledger handle. Capture
    // the budget separately so that this is not mistaken for an uninitialized
    // ledger when classifying new online tasks.
    let enqueue_budget = if let Some(ledger) = ledger.as_ref() {
        let (device, folder) =
            budget_remaining_for_adapter(ledger, &budget_caps, &scope_id, "markdownize")?;
        EnqueueBudget::Available { device, folder }
    } else if args.offline {
        let path = ledger_db_path(repo.control());
        let (month, snapshot) = capture_search_budget_observation(&path, repo.control())?;
        if let Some(snapshot) = snapshot {
            let total = |scope: Option<&str>, adapter: Option<&str>| {
                snapshot
                    .month_total(scope, adapter, &month)
                    .map_err(|error| ledger_snapshot_to_kio(&path.display().to_string(), error))
            };
            let (device, folder) = budget_remaining_from_totals(
                &budget_caps,
                "markdownize",
                total(None, None)?,
                total(Some(&scope_id), None)?,
                total(None, Some("markdownize"))?,
            );
            EnqueueBudget::Available { device, folder }
        } else {
            EnqueueBudget::Uninitialized
        }
    } else {
        EnqueueBudget::Uninitialized
    };

    // R18-2: release the ledger reservation of DELETED / renamed files' orphaned
    // tasks. R17-3's enqueue supersede (`enqueue_online_placeholder_task`) releases
    // a stale task's reservation only when the SAME path is re-scanned
    // (`task.input_path == candidate.input_path`); a DELETED or renamed file never
    // reappears as a scan candidate, so its still-open ledger row would eat the
    // per-adapter markdownize cap for the rest of the month and falsely pause
    // unrelated tasks (the same harm R17-3 fixed, surviving on the delete path).
    // With the full live-candidate set in hand, retire + release any
    // reservation-bearing online markdownize task whose input_path is no longer a
    // live candidate. Runs before the enqueue loop so the freed cap is available to
    // this index's tasks.
    //
    // QA2/QA3 (step4b-contract-tests-p3a.md §A, 04 §5.2/§5.3): a
    // reservation-bearing send failure no longer always lands `Failed` —
    // rate_limit stays `Pending` and auth_error lands `Paused`. Admit both
    // statuses here too (`is_reservation_bearing_send_failure` already scopes
    // this to the 4 kinds that actually carry an open reservation, so a fresh
    // never-attempted Pending task or a budget/secrets Paused hold — neither
    // reservation-bearing — is unaffected).
    if let Some(ledger) = ledger.as_ref() {
        let online_profile = online_markdownize_profile_for(repo)?;
        let placeholder_output_ref = online_output_ref(&online_profile.adapter_id);
        let reservation_scope_id = repo.scope_identity()?.scope_id;
        let live_paths: BTreeSet<&str> = preview
            .candidates
            .iter()
            .filter(|candidate| !candidate.ignored && candidate.media_type != "inode/directory")
            .map(|candidate| candidate.input_path.as_str())
            .collect();
        let mut orphan_ids = BTreeSet::new();
        for task in task_store.all().map_err(pipeline_to_kio)? {
            let orphaned = task.task_type == TaskType::Markdownize
                && targets_standard_online_markdownize(
                    task_store.kio_dir(),
                    &task,
                    &placeholder_output_ref,
                )
                && matches!(
                    task.status,
                    TaskStatus::Failed | TaskStatus::Pending | TaskStatus::Paused
                )
                && is_reservation_bearing_send_failure(&task)
                && !live_paths.contains(task.input_path.as_str());
            if !orphaned {
                continue;
            }
            let key = task_ledger_key(
                &reservation_scope_id,
                "markdownize",
                &task.input_hash,
                &online_profile.tool_profile_hash,
            );
            release_task_charge_if_open(ledger, &key)?;
            orphan_ids.insert(task.task_id);
        }
        task_store
            .update_matching(|task| {
                if !orphan_ids.contains(&task.task_id) {
                    return false;
                }
                retire_online_task(task);
                true
            })
            .map_err(pipeline_to_kio)?;
    }

    let mut result = IndexPipelineResult {
        network_allowed,
        ..IndexPipelineResult::default()
    };
    let unsupported_store = UnsupportedInputStore::new(repo.kio_dir());
    let mut unsupported_by_path = unsupported_store
        .latest_by_path()
        .map_err(pipeline_to_kio)?
        .into_iter()
        .map(|entry| (entry.path.clone(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut unsupported_paths = unsupported_by_path
        .values()
        .filter(|entry| entry.reason == UNSUPPORTED_REASON_UNRECOGNIZED_BINARY)
        .map(|entry| entry.path.clone())
        .collect::<BTreeSet<_>>();
    // 07 §5.1: mirrors `unsupported_paths` above, scoped to the
    // office-conversion-unavailable reason so `enqueue_online_placeholder_task`
    // can record the resolved-transition once a converter becomes available.
    let mut office_conversion_unavailable_paths = unsupported_by_path
        .values()
        .filter(|entry| entry.reason == OFFICE_CONVERSION_UNAVAILABLE_REASON)
        .map(|entry| entry.path.clone())
        .collect::<BTreeSet<_>>();
    // R12-2: the documented `adapter.policy.max_input_bytes` input gate (07 §7.1.2 —
    // "Kio 側の入力制御" is an MVP contract). Scope config wins over user config,
    // default 100 MB. A file larger than the cap is never handed to the Markdownize
    // adapter (below); it stays archived but unenriched, and the count is disclosed.
    let max_input_bytes = effective_max_input_bytes(repo);
    // R12-1: the documented `[markdownize.incremental]` overrides (were hardcoded).
    let incremental_config = effective_incremental_config(repo)?;
    let head_ocr_normalizes = repo
        .head_commit_hash()?
        .map(|head| read_head_tree_for_rebuild(repo, &head))
        .transpose()?
        .map(|tree| {
            tree.entries
                .into_iter()
                .filter_map(|entry| {
                    entry
                        .normalize
                        .map(|normalize| (entry.path, (entry.raw_hash, normalize)))
                })
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();

    for candidate in preview
        .candidates
        .iter()
        .filter(|candidate| !candidate.ignored && candidate.media_type != "inode/directory")
    {
        if candidate.size_bytes > max_input_bytes {
            result.skipped_oversized_files += 1;
            repo.control().append_event_log_in_session(
                repo.lock_session(),
                "KIO-I-INDEX-INPUT-OVERSIZED-001",
                "input file exceeds adapter.policy.max_input_bytes; skipped adapter processing",
                json!({
                    "size_bytes": candidate.size_bytes,
                    "max_input_bytes": max_input_bytes,
                }),
            )?;
            continue;
        }
        let path = repository_input_display_path(repo, &candidate.input_path);
        if !repository_scan_policy_allows_file(repo, &candidate.input_path)
            .map_err(pipeline_to_kio)?
        {
            repo.control().append_event_log_in_session(
                repo.lock_session(),
                "KIO-I-INDEX-INPUT-POLICY-CHANGED-001",
                "input is no longer authorized by current scope policy; skipped",
                json!({ "input_path": candidate.input_path }),
            )?;
            result.failed_files += 1;
            continue;
        }
        let verified =
            match read_repository_scan_input(repo, &candidate.input_path, max_input_bytes) {
                Ok(verified) => verified,
                Err(error) => {
                    repo.control().append_event_log_in_session(
                        repo.lock_session(),
                        "KIO-I-INDEX-INPUT-CHANGED-001",
                        "input could not be rebound to a regular bounded scope child; skipped",
                        json!({ "input_path": candidate.input_path }),
                    )?;
                    let _ = error;
                    result.failed_files += 1;
                    continue;
                }
            };
        // R15-8: `candidate.raw_hash` was computed at SCAN time; `bytes` is read HERE. If
        // the file was externally edited in that window, persisting the normalized
        // instance under the stale SCAN hash while the closing snapshot hashes the CURRENT
        // bytes for the tree entry would diverge their identities (the offline twin of
        // R14-2). Re-hash what we actually read and, on a mismatch, SKIP this candidate —
        // the next clean index re-scans and normalizes it under its correct identity. The
        // window is one store-lock-held `kio index` plus an external write, so this is a
        // rare defensive guard. (No deterministic test: reproducing it needs an external
        // edit landing inside the lock-held critical section between the scan and this
        // read, which no public seam exposes.)
        let current_hash = verified.raw_hash.clone();
        if let Some(scan_hash) = &candidate.raw_hash
            && scan_hash != &current_hash
        {
            repo.control().append_event_log_in_session(
                repo.lock_session(),
                "KIO-I-INDEX-INPUT-CHANGED-001",
                "input file changed between scan and normalize; skipped to preserve \
                     content-addressing (re-run index)",
                json!({ "input_path": candidate.input_path }),
            )?;
            result.failed_files += 1;
            continue;
        }
        let raw_hash = current_hash;
        // Reject before Prepare/Markdownize writes any derived state. The closing
        // snapshot independently rechecks every staged raw under the store lock.
        ensure_raw_ingest_allowed(repo, &raw_hash)?;
        let prepare = prepare_units_from_bytes_with_control(
            PrepareStageBytesRequest {
                raw_hash: &raw_hash,
                media_type: &candidate.media_type,
                input_path: &path.display().to_string(),
                tool_profile_hash: &prepare_profile_hash,
                bytes: &verified.bytes,
            },
            repo.control(),
        )
        .map_err(pipeline_to_kio)?;

        write_prepared_objects(
            repo,
            &prepare.prepared_units,
            &prepare.prepared_object_hashes,
            &verified.bytes,
            &candidate.media_type,
        )?;

        if let Some(normalize) = reusable_current_ocr_normalize(
            adapter_settings,
            repo,
            &head_ocr_normalizes,
            &candidate.input_path,
            &raw_hash,
            &candidate.media_type,
        )? {
            result.normalize_by_path.insert(
                candidate.input_path.clone(),
                PendingNormalizeRef {
                    expected_raw_hash: raw_hash.clone(),
                    normalize,
                },
            );
            result.normalized_files += 1;
            continue;
        }

        // Standalone raster images are locally indexable as immutable image
        // objects. Their normalized unit contains only a kio:// image reference,
        // so no binary bytes are treated as Markdown and local image-vector
        // enrichment can discover the pixels through the ordinary chunk path.
        if matches!(
            candidate.media_type.as_str(),
            "image/png" | "image/jpeg" | "image/webp"
        ) {
            let native_profile_hash = image_ingest::native_image_profile_hash();
            let native_instance = kio_pipeline::markdownize::normalized_instance_dir(
                repo.kio_dir(),
                &raw_hash,
                &native_profile_hash,
                0,
            );
            let normalize = if native_instance.is_dir() {
                // A same raw/profile/gen instance is immutable authority. Reuse
                // only after the normal validated loader rebinds its manifest.
                NormalizeRef {
                    tool_profile_hash: native_profile_hash,
                    r#gen: 0,
                    manifest_hash: compute_manifest_hash(
                        repo,
                        &raw_hash,
                        &image_ingest::native_image_profile_hash(),
                        0,
                    )?,
                }
            } else {
                image_ingest::ingest_standalone_image(
                    repo,
                    &candidate.input_path,
                    &raw_hash,
                    &verified.bytes,
                    &candidate.media_type,
                    &prepare_profile_hash,
                    &markdown_profile_hash,
                )?
                .normalize
            };
            result.normalize_by_path.insert(
                candidate.input_path.clone(),
                PendingNormalizeRef {
                    expected_raw_hash: raw_hash.clone(),
                    normalize,
                },
            );
            result.normalized_files += 1;
            // Native image ingestion establishes the pixel authority locally;
            // OCR remains an optional later enrichment and has its own online
            // task/output identity, so it cannot replace this image object.
            enqueue_online_placeholder_task(
                adapter_settings,
                repo,
                &task_store,
                candidate,
                &raw_hash,
                &scope_id,
                network_allowed,
                args,
                &now,
                &mut result,
                ledger.as_ref(),
                enqueue_budget,
                &budget_caps,
                &unsupported_store,
                &mut unsupported_by_path,
                &mut office_conversion_unavailable_paths,
            )?;
            continue;
        }

        if prepare.prepared_units.is_empty() {
            // R20-5: this media has no locally-extractable text — a text-layer-less/scanned
            // PDF, or (after R20-6) an image / OOXML / unrecognized binary. It needs ONLINE
            // OCR, so route it through the SAME enqueue path as a text-layer PDF's online
            // enhancement: an idempotent task carrying the executable online output_ref,
            // gated by secret / budget / network opt-in. The old branch appended a fixed
            // "pending:scanned_pdf_without_text_layer" output_ref that NO executor consumed
            // (`execute_pending_markdownize_tasks` filters on the online output_ref) and had
            // no idempotency guard (04 §5.5), so it silently duplicated on every idle
            // re-index and the file never reached OCR.
            //
            // R21-4: only a RECOGNIZED OCR-able medium (PDF / image / OOXML — docs/07 §5.2)
            // is worth an online OCR task. An unrecognized `application/octet-stream` binary
            // (a .sqlite, a .zip, a compiled blob) is not a document and can never be OCR'd,
            // so enqueuing one only pollutes pending enrichment forever. Skip it.
            if candidate.media_type != "application/octet-stream" {
                if unsupported_paths.remove(&candidate.input_path) {
                    record_unsupported_if_changed(
                        &unsupported_store,
                        &mut unsupported_by_path,
                        UnsupportedInputDisposition {
                            path: candidate.input_path.clone(),
                            raw_hash: raw_hash.clone(),
                            media_type: candidate.media_type.clone(),
                            size_bytes: verified.size_bytes,
                            reason: UNSUPPORTED_REASON_RESOLVED.to_owned(),
                        },
                    )?;
                }
                enqueue_online_placeholder_task(
                    adapter_settings,
                    repo,
                    &task_store,
                    candidate,
                    &raw_hash,
                    &scope_id,
                    network_allowed,
                    args,
                    &now,
                    &mut result,
                    ledger.as_ref(),
                    enqueue_budget,
                    &budget_caps,
                    &unsupported_store,
                    &mut unsupported_by_path,
                    &mut office_conversion_unavailable_paths,
                )?;
            } else {
                // R22-4: the R21-4 skip left NO trace — no task, no counter, no event — so a
                // real document whose extension is merely missing from the MIME table (a
                // `.bmp` / `.tiff` / `.heic` image, a legacy `.doc` / `.xls`, an `.epub`)
                // vanished from enrichment while `index_status` still reported
                // `enriched_ratio: 1.0` and `kio status` showed the file as `unchanged`. The
                // bytes are archived, but nothing is searchable and no recovery command has
                // anything to act on. Disclose it exactly the way the sibling oversized-input
                // gate does: a counter plus an INFO event (no new error code — docs frozen).
                result.skipped_unrecognized_binary_files += 1;
                record_unsupported_if_changed(
                    &unsupported_store,
                    &mut unsupported_by_path,
                    UnsupportedInputDisposition {
                        path: candidate.input_path.clone(),
                        raw_hash: raw_hash.clone(),
                        media_type: candidate.media_type.clone(),
                        size_bytes: verified.size_bytes,
                        reason: UNSUPPORTED_REASON_UNRECOGNIZED_BINARY.to_owned(),
                    },
                )?;
                unsupported_paths.insert(candidate.input_path.clone());
                repo.control().append_event_log_in_session(repo.lock_session(),
                    "KIO-I-INDEX-INPUT-UNRECOGNIZED-BINARY-001",
                    "binary input has no locally-extractable text and no OCR-able media type; archived but not enriched",
                    json!({
                        "media_type": candidate.media_type,
                        "size_bytes": candidate.size_bytes,
                    }),
                )?;
            }
            continue;
        }

        if unsupported_paths.remove(&candidate.input_path) {
            record_unsupported_if_changed(
                &unsupported_store,
                &mut unsupported_by_path,
                UnsupportedInputDisposition {
                    path: candidate.input_path.clone(),
                    raw_hash: raw_hash.clone(),
                    media_type: candidate.media_type.clone(),
                    size_bytes: verified.size_bytes,
                    reason: UNSUPPORTED_REASON_RESOLVED.to_owned(),
                },
            )?;
        }

        let output_ref = normalized_output_ref(repo, &raw_hash, &markdown_profile_hash, 0);
        if task_store
            .done_output_for(&raw_hash, &output_ref)
            .map_err(pipeline_to_kio)?
            .is_some()
        {
            // QB41 (03 §2.1's second legal gen+1 path, 07 §5.1): raw_hash is
            // UNCHANGED (a done/partial gen-0 instance already exists for it),
            // but `prepare` above (already computed fresh THIS pass, for THIS
            // candidate -- no extra corpus scan) may still disagree with that
            // instance's per-unit `prepared_hash`: a prepare profile /
            // renderer version change (the Office converted-PDF path is the
            // motivating case) can change `prepared_hash` without changing
            // raw_hash at all. Detect and gate it BEFORE the unconditional
            // reuse below would otherwise silently keep serving the STALE
            // generation forever.
            if let Some(new_gen) = prepared_hash_drift_new_gen(
                repo.kio_dir(),
                &raw_hash,
                &markdown_profile_hash,
                &prepare.prepared_units,
                repo.control(),
            ) {
                // Mirrors `kio reindex --force`'s confirmation gate exactly
                // (same error code / exit code / `--yes` flag) -- this
                // codebase has no real interactive (TTY) confirmation prompt
                // anywhere yet (`run_index`'s own top-level approval flow
                // says so explicitly), so non-interactive `--yes` is the only
                // accepted consent, matching that existing precedent rather
                // than inventing a new interaction model here.
                if !args.yes {
                    return Err(KioError::new(
                        "KIO-E-CONFIRM-REJECTED-001",
                        "prepared_hash changed for an unchanged raw_hash (prepare profile / \
                         renderer drift); re-markdownize at a new generation requires \
                         confirmation -- pass --yes in non-interactive mode",
                        json!({
                            "input_path": candidate.input_path,
                            "new_gen": new_gen,
                        }),
                        ExitCode::ConfirmationRejected,
                    ));
                }
                create_prepared_hash_drift_instance(
                    repo,
                    &task_store,
                    markdown_adapter.as_ref(),
                    candidate,
                    &path,
                    &raw_hash,
                    &markdown_profile_hash,
                    &prepare.prepared_units,
                    &verified.bytes,
                    new_gen,
                    &now,
                    &mut result,
                )?;
                continue;
            }
            // PB04: this candidate's gen=0 output is already `done`
            // (verified above), so its manifest.json is hashable now.
            let manifest_hash = compute_manifest_hash(repo, &raw_hash, &markdown_profile_hash, 0)?;
            result.normalize_by_path.insert(
                candidate.input_path.clone(),
                PendingNormalizeRef {
                    expected_raw_hash: raw_hash.clone(),
                    normalize: NormalizeRef {
                        tool_profile_hash: markdown_profile_hash.clone(),
                        r#gen: 0,
                        manifest_hash,
                    },
                },
            );
            result.normalized_files += 1;
            // R21-4: an online OCR "enhancement" task after a SUCCESSFUL local markdownize
            // is only meaningful for a real text-layer PDF (docs/07 §8) or, per 07 §5.1, an
            // Office file whose converter resolved (the enqueue gate itself decides whether
            // to actually enqueue -- item 1's `enqueue_online_placeholder_task` early-return).
            // A non-`.md`/non-code TEXT file folded to `application/octet-stream` (a `.yaml` /
            // `.json` / `Dockerfile`, or an uppercase-extension text-native file) is already
            // fully and finally handled by the local passthrough — enqueuing an OCR task
            // shipped its bytes to the external API under `--online` (R9-2 routing violation)
            // and, when offline, left a permanent phantom pending that deflated enriched_ratio.
            if candidate.media_type == "application/pdf" || is_office_media(&candidate.media_type) {
                enqueue_online_placeholder_task(
                    adapter_settings,
                    repo,
                    &task_store,
                    candidate,
                    &raw_hash,
                    &scope_id,
                    network_allowed,
                    args,
                    &now,
                    &mut result,
                    ledger.as_ref(),
                    enqueue_budget,
                    &budget_caps,
                    &unsupported_store,
                    &mut unsupported_by_path,
                    &mut office_conversion_unavailable_paths,
                )?;
            }
            continue;
        }

        // R14-1 (defense in depth): loading the prior instance for THIS candidate must
        // not abort the whole index and silently skip alphabetically-later files. On any
        // error (a store read fault, an unreadable prior instance the `Ok(None)` fix
        // above did not already absorb), degrade to no-previous = a Full re-send for this
        // candidate only, keeping the blast radius to the one file.
        let previous =
            previous_instance_for_path(&task_store, &candidate.input_path, &markdown_profile_hash)
                .unwrap_or_else(|_err| {
                    let _ = repo.control().append_event_log_in_session(
                        repo.lock_session(),
                        "KIO-I-INDEX-PREVIOUS-UNREADABLE-001",
                        "prior normalized instance unreadable; indexing this file as Full",
                        json!({ "input_path": candidate.input_path }),
                    );
                    None
                });
        let mapping = if incremental_config.enabled {
            previous
                .as_ref()
                .map(|previous| map_units(&previous.prepared_units, &prepare.prepared_units))
        } else {
            None
        };
        let incremental_hints = mapping
            .as_ref()
            .map(|mapping| incremental_hints_from_mapping(mapping, &prepare.prepared_units))
            .unwrap_or_else(|| all_changed_hints(&prepare.prepared_units));
        // R12-1: `enabled = false` disables incremental entirely — always full mode
        // (05 / docs/10:537). Otherwise the effective threshold / max_consecutive
        // (were hardcoded 0.30 / 5) drive the documented decision.
        let mode_decision = if incremental_config.enabled {
            choose_markdownize_mode(&IncrementalModeInput {
                has_previous_done_run: previous.is_some(),
                raw_hash_only_changed: true,
                adapter_capabilities: markdown_profile.capability_flags.clone(),
                change_rate: mapping
                    .as_ref()
                    .map(|mapping| mapping.change_rate)
                    .unwrap_or(1.0),
                threshold: incremental_config.threshold,
                consecutive_incremental_count: consecutive_incremental_count(
                    &task_store,
                    &candidate.input_path,
                )?,
                max_consecutive_incremental: incremental_config.max_consecutive,
            })
        } else {
            IncrementalModeDecision {
                mode: MarkdownizeMode::Full,
                reason: Some("incremental_disabled".to_owned()),
            }
        };
        let mode = mode_decision.mode;
        let hints = prepared_unit_hints(&prepare.prepared_units);
        let adapter_previous = previous
            .as_ref()
            .map(|previous| PreviousMarkdownizeContext {
                raw: RawInput {
                    raw_hash: previous.manifest.raw_hash.clone(),
                    path: Some(
                        repo.canonical_root()
                            .join(&candidate.input_path)
                            .display()
                            .to_string(),
                    ),
                },
                normalized_units: previous
                    .units
                    .iter()
                    .map(normalized_unit_to_adapter_unit)
                    .collect(),
                tool_profile_hash: previous.manifest.tool_profile_hash.clone(),
            });
        let request = MarkdownizeRequest {
            raw: RawInput {
                raw_hash: raw_hash.clone(),
                path: Some(path.display().to_string()),
            },
            media_type: candidate.media_type.clone(),
            prepared_unit_hint: Some(hints),
            mode: adapter_mode(mode),
            previous: (mode == MarkdownizeMode::Incremental)
                .then_some(adapter_previous)
                .flatten(),
            hints: (mode == MarkdownizeMode::Incremental)
                .then(|| adapter_hints(&incremental_hints)),
            // Offline (deterministic) markdownize path: the builtin adapter ignores
            // page scoping (R15-5 concerns only the real Mistral client).
            restrict_to_hint_pages: false,
            bbox_annotation_enabled: false,
            tool_profile_hash: markdown_profile_hash.clone(),
            spec_version: 1,
            // QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880): this is
            // the free/local (no ledger charge) offline Markdownize path —
            // there is no `intent_token` to carry.
            idempotency_token: None,
        };

        let mut response = offline_markdownize_from_verified_bytes(
            markdown_adapter.as_ref(),
            request.clone(),
            &verified.bytes,
            repo.control(),
        )
        .map_err(adapter_to_kio)?;
        let mut final_mode = mode;
        let mut fallback_reason = mode_decision.reason.clone();
        let validation = if response.fallback_to_full {
            Err(kio_pipeline::PipelineError::contract(
                "KIO-E-ADAPTER-CONTRACT-001",
                "adapter_requested_full_fallback",
            ))
        } else {
            validate_markdownize_response(&response, &incremental_hints, &prepare.prepared_units)
        };
        if let Err(error) = validation {
            if mode != MarkdownizeMode::Incremental {
                append_failed_markdownize_task(
                    repo,
                    &task_store,
                    candidate,
                    &raw_hash,
                    &output_ref,
                    "contract_violation",
                    &now,
                )?;
                result.failed_files += 1;
                // The validation verdict used to be silently discarded here,
                // which made every such failure a blind "contract_violation"
                // task (2026-07-23 fixture registration: 48 opaque failures
                // whose real cause — unfenced raw HTML in octet-text
                // passthrough — took a live probe to recover). Keep the
                // task-record shape, but preserve the actual reason in the
                // event log.
                repo.control().append_event_log_in_session(repo.lock_session(),
                    "KIO-E-ADAPTER-CONTRACT-001",
                    "offline markdownize response failed acceptance; task recorded as contract_violation",
                    json!({
                        "input_path": candidate.input_path,
                        "detail": error.to_string(),
                    }),
                )?;
                continue;
            }
            let mut full_request = request;
            full_request.mode = AdapterMarkdownizeMode::Full;
            full_request.previous = None;
            full_request.hints = None;
            let fallback_response = offline_markdownize_from_verified_bytes(
                markdown_adapter.as_ref(),
                full_request,
                &verified.bytes,
                repo.control(),
            )
            .map_err(adapter_to_kio)
            .and_then(|response| {
                validate_markdownize_response(
                    &response,
                    &incremental_hints,
                    &prepare.prepared_units,
                )
                .map_err(pipeline_to_kio)?;
                Ok(response)
            });
            let Ok(fallback_response) = fallback_response else {
                append_failed_markdownize_task(
                    repo,
                    &task_store,
                    candidate,
                    &raw_hash,
                    &output_ref,
                    "full_fallback_failed",
                    &now,
                )?;
                result.failed_files += 1;
                continue;
            };
            response = fallback_response;
            final_mode = MarkdownizeMode::Full;
            fallback_reason = Some("full_fallback_after_incremental_reject".to_owned());
        }

        let generated_at = repo.control().now_utc_seconds();
        let run_id = format!("run_{}", new_ulid(repo.canonical_root()));
        let units = normalized_units_from_response(
            repo,
            &candidate.media_type,
            &verified.bytes,
            &response,
            &prepare.prepared_units,
            previous.as_ref(),
            &raw_hash,
            &markdown_profile_hash,
            0,
            final_mode,
            &generated_at,
        )
        .map_err(NormalizedUnitsError::into_kio)?;
        let manifest = manifest_from_units(
            &prepare.prepared_units,
            &units,
            &raw_hash,
            &markdown_profile_hash,
            0,
            previous.as_ref().map(|previous| previous.manifest.r#gen),
            &run_id,
            &generated_at,
            // The local deterministic markdownize always returns every unit, so this
            // kind is inert here; a hypothetical missing unit would be a
            // non-retryable library/contract fault rather than a transient.
            RetryErrorKind::ContractViolation,
        );
        let stamped_manifest = persist_normalized_instance_with_control(
            repo.kio_dir(),
            &manifest,
            &units,
            repo.control(),
        )
        .map_err(pipeline_to_kio)?;
        // The returned manifest includes the immutable normalized-unit CAS
        // pins that the writer just committed; derive the tree pin from those
        // exact bytes rather than the pre-persist nullable manifest.
        let manifest_hash = hash_and_write_manifest_object(repo, &stamped_manifest)?;
        let task = task_descriptor(
            repo,
            TaskType::Markdownize,
            Some(mode),
            candidate,
            &raw_hash,
            &output_ref,
            TaskStatus::Done,
            fallback_reason.as_deref(),
            &generated_at,
        );
        let mut task = task;
        task.mode = Some(final_mode);
        task.previous_raw_hash = previous
            .as_ref()
            .map(|previous| previous.manifest.raw_hash.clone());
        task.parent_run_id = previous
            .as_ref()
            .map(|previous| previous.manifest.run_id.clone());
        task.changed_unit_keys = incremental_hints.changed_unit_keys.clone();
        task.unit_keys = Some(
            prepare
                .prepared_units
                .iter()
                .map(|unit| unit.unit_key.clone())
                .collect(),
        );
        task_store.append(&task).map_err(pipeline_to_kio)?;
        result.normalize_by_path.insert(
            candidate.input_path.clone(),
            PendingNormalizeRef {
                expected_raw_hash: raw_hash.clone(),
                normalize: NormalizeRef {
                    tool_profile_hash: markdown_profile_hash.clone(),
                    r#gen: 0,
                    manifest_hash,
                },
            },
        );
        result.normalized_files += 1;
        // R21-4: only a real text-layer PDF, or (07 §5.1) an Office file whose converter
        // resolved, warrants an online OCR "enhancement" task after a successful local
        // markdownize -- the enqueue gate itself (item 1) decides whether to actually
        // enqueue an Office file. A TEXT file folded to `application/octet-stream`
        // (`.yaml`/`.json`/`Dockerfile`/uppercase-extension text-native) is fully handled
        // locally; enqueuing OCR sent its bytes online (R9-2 violation) or left an offline
        // phantom pending.
        if candidate.media_type == "application/pdf" || is_office_media(&candidate.media_type) {
            enqueue_online_placeholder_task(
                adapter_settings,
                repo,
                &task_store,
                candidate,
                &raw_hash,
                &scope_id,
                network_allowed,
                args,
                &now,
                &mut result,
                ledger.as_ref(),
                enqueue_budget,
                &budget_caps,
                &unsupported_store,
                &mut unsupported_by_path,
                &mut office_conversion_unavailable_paths,
            )?;
        }
    }
    // Stage 3: drain local-HTTP OCR only when this invocation is not offline and
    // the active local peer has an exact device+scope network grant. The task is
    // created first and drained second, so a crash mid-OCR leaves a Pending task
    // that the next authorized index invocation can pick up.
    let executed = execute_pending_offline_markdownize_tasks(
        adapter_settings,
        repo,
        &task_store,
        &mut result.normalize_by_path,
        network_allowed,
    )?;
    result.pending_online_tasks = result.pending_online_tasks.saturating_sub(executed);
    Ok(result)
}

/// QB41 (03 §2.1's second legal gen+1 path, 07 §5.1): the newest existing
/// normalized instance for `(raw_hash, tool_profile_hash)`, found by a bounded
/// linear probe from gen 0 upward. Mirrors `kio reindex --force`'s own gen-walk
/// tolerance (`copy_normalized_instance_gen`'s caller, `run_reindex`): a
/// missing OR unreadable gen N is treated as "N-1 was the latest", the same
/// forward-degradation this codebase already accepts elsewhere for a
/// corrupt/partial gen (R17-2's doc comment on the analogous reindex loop).
fn latest_normalized_instance(
    kio_dir: &Path,
    raw_hash: &str,
    tool_profile_hash: &str,
    control: &ControlHandle,
) -> kio_pipeline::Result<ValidatedNormalizedInstance> {
    // Gen numbers are always small in practice (only `kio reindex --force` and
    // this QB41 path ever create gen > 0) -- this bound only protects against a
    // pathological/adversarial store, not a real workload.
    const MAX_GEN_PROBE: u64 = 4096;
    let mut latest = load_validated_normalized_instance_with_control(
        kio_dir,
        raw_hash,
        tool_profile_hash,
        0,
        control,
    )?;
    for r#gen in 1..MAX_GEN_PROBE {
        match load_validated_normalized_instance_with_control(
            kio_dir,
            raw_hash,
            tool_profile_hash,
            r#gen,
            control,
        ) {
            Ok(instance) => latest = instance,
            Err(_) => break,
        }
    }
    Ok(latest)
}

/// QB41: `Some(new_gen)` when the LATEST existing normalized instance's
/// per-unit prepared identity disagrees with `fresh_prepared_units` (the
/// SAME-pass, already-computed fresh Prepare output for this candidate -- no
/// extra corpus scan) -- a prepare profile / renderer version change drove a
/// prepared identity change without any `raw_hash` change at all (03 §2.1:
/// "`gen = 現最大 + 1` の新 instance を作れるのは `kio reindex --force` と、prepare
/// profile / renderer 変更による `prepared_hash` 変化が駆動する再 Markdownize...
/// だけ"; 07 §5.1's Office converted-PDF renderer-drift is the motivating
/// case). `None` when they agree (the ordinary reuse path is unaffected) OR
/// when the existing instance cannot be loaded at all (graceful degrade --
/// this detector must never be a NEW source of a hard `kio index` failure for
/// a corruption it did not cause; the existing reuse/rebuild paths already
/// handle that case on their own terms).
fn prepared_hash_drift_new_gen(
    kio_dir: &Path,
    raw_hash: &str,
    tool_profile_hash: &str,
    fresh_prepared_units: &[PreparedUnit],
    control: &ControlHandle,
) -> Option<u64> {
    // Guard (a) — defense in depth: an EMPTY fresh prepare is never a drift
    // verdict. The local extractor legitimately yields zero units for content
    // it cannot read (R20-4 garbage-gates every FlateDecode-compressed
    // real-world PDF, including real soffice-converted Office output) while a
    // done ONLINE instance discovered its units from the OCR response —
    // comparing those would flag such files as "drifted" on every `kio index`
    // (exit 9 without --yes; with --yes an offline gen+1 built from unreadable
    // input would shadow the OCR content as the latest generation). TODAY this
    // state cannot reach the caller at all — run_index_pipeline's R20-5
    // empty-prepare arm peels these candidates off into the online-enqueue
    // path before the done-instance branch (verified by revert-testing
    // office_07 with this guard disabled) — but that reachability is one
    // refactor (or a FlateDecode-capable extractor changing what "empty"
    // means) away from silently flipping, so the invariant is enforced here
    // where the verdict is minted, not assumed from caller topology.
    if fresh_prepared_units.is_empty() {
        return None;
    }
    let latest = latest_normalized_instance(kio_dir, raw_hash, tool_profile_hash, control).ok()?;
    let existing: BTreeMap<&str, (&str, &str)> = latest
        .manifest
        .units
        .iter()
        .map(|unit| {
            (
                unit.unit_key.as_str(),
                (
                    unit.prepared_hash.as_str(),
                    unit.preparation_profile_hash.as_str(),
                ),
            )
        })
        .collect();
    let fresh: BTreeMap<&str, (&str, &str)> = fresh_prepared_units
        .iter()
        .map(|unit| {
            (
                unit.unit_key.as_str(),
                (
                    unit.prepared_hash.as_str(),
                    unit.preparation_profile_hash.as_str(),
                ),
            )
        })
        .collect();
    let existing_profiles = existing
        .values()
        .map(|(_, profile)| *profile)
        .collect::<BTreeSet<_>>();
    let fresh_profiles = fresh
        .values()
        .map(|(_, profile)| *profile)
        .collect::<BTreeSet<_>>();
    let preparation_profile_changed = existing_profiles != fresh_profiles;
    // Guard (b): an identity disagreement on an IDENTICAL unit-key set is a
    // safe renderer-drift verdict. A key-set difference without a profile
    // change is indistinguishable
    // from an OCR-discovered manifest whose unit set never came from the local
    // prepare basis (OCR-from-scratch discovery mints placeholder prepared
    // hashes for pages the extractor never produced) — treating that as drift
    // would re-prompt on every index instead of once. With the current
    // hint-echoing adapters a key-set mismatch is not end-to-end constructible
    // (hints round-trip verbatim; OCR-from-scratch implies the empty-prepare
    // arm), so like guard (a) this enforces the invariant at the mint rather
    // than trusting adapter topology to keep it true. A renderer update that
    // changes pagination therefore stays `kio reindex --force` territory
    // (documented residual, step4b backlog).
    if existing.keys().ne(fresh.keys()) && !preparation_profile_changed {
        return None;
    }
    (preparation_profile_changed || existing != fresh)
        .then(|| latest.manifest.r#gen.saturating_add(1))
}

/// QB41 (03 §2.1's second legal gen+1 path): create a FRESH normalized
/// instance at `new_gen` for a `raw_hash` whose prepare output drifted from
/// the existing instance's (`prepared_hash_drift_new_gen` already confirmed
/// the drift, and the caller already gated confirmation -- `--yes`, mirroring
/// `kio reindex --force`'s `KIO-E-CONFIRM-REJECTED-001` pattern). Deliberately
/// FULL mode only (no incremental): this is a rare, renderer-drift-driven
/// event, not a hot path, and the OLD instance's per-unit content has no sound
/// correspondence to the NEW prepare output for incremental-reuse purposes
/// (mirrors `execute_online_markdownize_task`'s office incremental-skip doc,
/// the analogous online-side reasoning). The OLD gen is left completely
/// untouched (first-instance-wins) -- only a NEW `g<new_gen>` directory is
/// created; `write_prepared_objects` already durably wrote the (gen-independent,
/// content-addressed) prepared CAS objects earlier in this same candidate's
/// pass, so this function does not repeat that step.
///
/// Scope-trim (documented, not a silent gap -- see the implementation report):
/// this does not ALSO drive a fresh online-adapter send for the new
/// generation. `enqueue_online_placeholder_task`'s idempotency keys off
/// `(input_path, input_hash)` only (not gen), so an already-Done online task
/// for this raw_hash would dedup-skip a fresh enqueue even though its content
/// is now stale for `new_gen`. Making the online lane gen-aware is a larger,
/// independent change deferred out of this pass.
#[allow(clippy::too_many_arguments)]
fn create_prepared_hash_drift_instance(
    repo: &Repository,
    task_store: &TaskStore,
    markdown_adapter: &dyn MarkdownizeAdapter,
    candidate: &ScanCandidate,
    path: &Path,
    raw_hash: &str,
    tool_profile_hash: &str,
    fresh_prepared_units: &[PreparedUnit],
    verified_bytes: &[u8],
    new_gen: u64,
    now: &str,
    result: &mut IndexPipelineResult,
) -> Result<()> {
    let output_ref = normalized_output_ref(repo, raw_hash, tool_profile_hash, new_gen);
    let request = MarkdownizeRequest {
        raw: RawInput {
            raw_hash: raw_hash.to_owned(),
            path: Some(path.display().to_string()),
        },
        media_type: candidate.media_type.clone(),
        prepared_unit_hint: Some(prepared_unit_hints(fresh_prepared_units)),
        mode: AdapterMarkdownizeMode::Full,
        previous: None,
        hints: None,
        restrict_to_hint_pages: false,
        bbox_annotation_enabled: false,
        tool_profile_hash: tool_profile_hash.to_owned(),
        spec_version: 1,
        idempotency_token: None,
    };
    let response = match offline_markdownize_from_verified_bytes(
        markdown_adapter,
        request,
        verified_bytes,
        repo.control(),
    ) {
        Ok(response) => response,
        Err(_) => {
            append_failed_markdownize_task(
                repo,
                task_store,
                candidate,
                raw_hash,
                &output_ref,
                "contract_violation",
                now,
            )?;
            result.failed_files += 1;
            return Ok(());
        }
    };
    let full_hints = all_changed_hints(fresh_prepared_units);
    if response.fallback_to_full
        || validate_markdownize_response(&response, &full_hints, fresh_prepared_units).is_err()
    {
        append_failed_markdownize_task(
            repo,
            task_store,
            candidate,
            raw_hash,
            &output_ref,
            "contract_violation",
            now,
        )?;
        result.failed_files += 1;
        return Ok(());
    }
    let generated_at = repo.control().now_utc_seconds();
    let run_id = format!("run_{}", new_ulid(repo.canonical_root()));
    let units = normalized_units_from_response(
        repo,
        &candidate.media_type,
        verified_bytes,
        &response,
        fresh_prepared_units,
        None,
        raw_hash,
        tool_profile_hash,
        new_gen,
        MarkdownizeMode::Full,
        &generated_at,
    )
    .map_err(NormalizedUnitsError::into_kio)?;
    let manifest = manifest_from_units(
        fresh_prepared_units,
        &units,
        raw_hash,
        tool_profile_hash,
        new_gen,
        Some(new_gen.saturating_sub(1)),
        &run_id,
        &generated_at,
        RetryErrorKind::ContractViolation,
    );
    let stamped_manifest =
        persist_normalized_instance_with_control(repo.kio_dir(), &manifest, &units, repo.control())
            .map_err(pipeline_to_kio)?;
    let manifest_hash = hash_and_write_manifest_object(repo, &stamped_manifest)?;
    let mut task = task_descriptor(
        repo,
        TaskType::Markdownize,
        Some(MarkdownizeMode::Full),
        candidate,
        raw_hash,
        &output_ref,
        TaskStatus::Done,
        Some("prepared_hash_drift_gen_bump"),
        &generated_at,
    );
    task.unit_keys = Some(
        fresh_prepared_units
            .iter()
            .map(|unit| unit.unit_key.clone())
            .collect(),
    );
    task_store.append(&task).map_err(pipeline_to_kio)?;
    result.normalize_by_path.insert(
        candidate.input_path.clone(),
        PendingNormalizeRef {
            expected_raw_hash: raw_hash.to_owned(),
            normalize: NormalizeRef {
                tool_profile_hash: tool_profile_hash.to_owned(),
                r#gen: new_gen,
                manifest_hash,
            },
        },
    );
    result.normalized_files += 1;
    Ok(())
}

/// Q2: crash-atomic write of a derived CAS byte object (prepared / image). Writes
/// to a uniquely-named temp file in the destination directory, fsyncs it, then
/// renames into place, so a crash / ENOSPC mid-write can never leave a partial
/// file under the final `sha256:` name. `cas::atomic_write` is `pub(crate)` and
/// unreachable from here, so this mirrors it locally. The caller keeps its
/// `if !path.exists()` dedup skip before calling this.
fn ensure_contained_directory_chain(kio_dir: &Path, target: &Path) -> Result<()> {
    let canonical_kio = kio_dir
        .canonicalize()
        .map_err(|error| KioError::io(error.to_string(), kio_dir.display().to_string()))?;
    let relative = target
        .strip_prefix(kio_dir)
        .map_err(|_| KioError::schema("derived object directory escapes the repository store"))?;
    let mut current = kio_dir.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(KioError::schema(
                "derived object directory contains a non-local path component",
            ));
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => {
                return Err(store_corrupt_error(
                    &current,
                    "derived object ancestor is not a real directory",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&current)
                .map_err(|error| KioError::io(error.to_string(), current.display().to_string()))?,
            Err(error) => {
                return Err(KioError::io(
                    error.to_string(),
                    current.display().to_string(),
                ));
            }
        }
        let canonical = current
            .canonicalize()
            .map_err(|error| KioError::io(error.to_string(), current.display().to_string()))?;
        if !canonical.starts_with(&canonical_kio) {
            return Err(store_corrupt_error(
                &current,
                "derived object ancestor resolves outside the repository store",
            ));
        }
    }
    Ok(())
}

fn atomic_write_cas_object(kio_dir: &Path, path: &Path, bytes: &[u8]) -> Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .ok_or_else(|| KioError::io("path has no parent", path.display().to_string()))?;
    ensure_contained_directory_chain(kio_dir, parent)?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".tmp-{}-{}-{}", process::id(), nanos, seq));
    match fs::symlink_metadata(path) {
        Ok(_) => return verify_exact_cas_object(path, bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(KioError::io(error.to_string(), path.display().to_string())),
    }
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        platform::configure_owner_only_file(&mut options);
        let mut file = options
            .open(&temp)
            .map_err(|err| KioError::io(err.to_string(), temp.display().to_string()))?;
        file.write_all(bytes)
            .map_err(|err| KioError::io(err.to_string(), temp.display().to_string()))?;
        file.sync_all()
            .map_err(|err| KioError::io(err.to_string(), temp.display().to_string()))?;
        drop(file);
        let mut permissions = fs::metadata(&temp)
            .map_err(|err| KioError::io(err.to_string(), temp.display().to_string()))?
            .permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&temp, permissions)
            .map_err(|err| KioError::io(err.to_string(), temp.display().to_string()))?;
        match fs::hard_link(&temp, path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                verify_exact_cas_object(path, bytes)?;
            }
            Err(error) => {
                return Err(KioError::io(error.to_string(), path.display().to_string()));
            }
        }
        fs::remove_file(&temp)
            .map_err(|err| KioError::io(err.to_string(), temp.display().to_string()))?;
        verify_exact_cas_object(path, bytes)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn verify_exact_cas_object(path: &Path, expected: &[u8]) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|err| KioError::io(err.to_string(), path.display().to_string()))?;
    if !metadata.file_type().is_file() || metadata.len() != expected.len() as u64 {
        return Err(store_corrupt_error(
            path,
            "content-addressed object is not the expected regular file",
        ));
    }
    platform::validate_exact_cas_links(&metadata, path)?;
    let mut file = fs::File::open(path)
        .map_err(|err| KioError::io(err.to_string(), path.display().to_string()))?;
    let opened = file
        .metadata()
        .map_err(|err| KioError::io(err.to_string(), path.display().to_string()))?;
    if !opened.is_file() || opened.len() != expected.len() as u64 {
        return Err(store_corrupt_error(
            path,
            "content-addressed object changed while it was verified",
        ));
    }
    let mut actual = Vec::with_capacity(expected.len());
    (&mut file)
        .take((expected.len() as u64).saturating_add(1))
        .read_to_end(&mut actual)
        .map_err(|err| KioError::io(err.to_string(), path.display().to_string()))?;
    if actual != expected || hash_bytes(&actual) != hash_bytes(expected) {
        return Err(store_corrupt_error(
            path,
            "content-addressed object bytes do not match their identity",
        ));
    }
    Ok(())
}

fn verify_existing_cas_object(
    kio_dir: &Path,
    subdir: &str,
    hash: &str,
    bytes: &[u8],
) -> Result<bool> {
    if let Some(path) = existing_cas_object_path(kio_dir, subdir, hash)? {
        let parent = path
            .parent()
            .ok_or_else(|| KioError::io("path has no parent", path.display().to_string()))?;
        ensure_contained_directory_chain(kio_dir, parent)?;
        verify_exact_cas_object(&path, bytes)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

fn write_canonical_cas_object_or_reuse(
    kio_dir: &Path,
    subdir: &str,
    hash: &str,
    bytes: &[u8],
) -> Result<()> {
    if verify_existing_cas_object(kio_dir, subdir, hash, bytes)? {
        return Ok(());
    }

    // Close the preflight/publication window as far as possible before entering the
    // atomic canonical writer. Its own create-new publication handles a canonical
    // race, which is re-verified below.
    if verify_existing_cas_object(kio_dir, subdir, hash, bytes)? {
        return Ok(());
    }

    let canonical = cas_object_path(kio_dir, subdir, hash)?;
    atomic_write_cas_object(kio_dir, &canonical, bytes)?;
    if !verify_existing_cas_object(kio_dir, subdir, hash, bytes)? {
        return Err(KioError::not_found(hash));
    }
    Ok(())
}

fn write_prepared_objects(
    repo: &Repository,
    prepared_units: &[PreparedUnit],
    prepared_hashes: &[String],
    bytes: &[u8],
    media_type: &str,
) -> Result<()> {
    if prepared_units.len() != prepared_hashes.len() {
        return Err(KioError::schema(
            "prepared unit and object hash cardinalities differ",
        ));
    }
    // 07 §5.1: a DOCX/PPTX unit is a page/slide of the CONVERTED PDF, not the
    // original file -- its declared `prepared_hash` (kio-pipeline's
    // `prepare_units_from_bytes`, which converts+extracts the same way) is the
    // per-page TEXT of that converted PDF, exactly like the `application/pdf`
    // branch below but sourced from a freshly reconverted PDF (deterministic
    // renderer output, 07 §5.1) instead of `bytes` itself. The `!is_empty()`
    // guard skips conversion entirely when Prepare already reported no units
    // (no converter resolved, or a non-Office/media caller) -- nothing to write
    // and no need to invoke (and possibly fail on) a converter for a no-op.
    // The `!prepared_units.is_empty()` guard (both arms) skips the page
    // recompute when Prepare already reported no units: there is nothing to
    // write, and re-running the extractor here can DISAGREE with Prepare's
    // verdict — a scanned/raster PDF whose compressed bytes contain a chance
    // "BT" passes the text-layer gate and lossy-scans to one garbage page,
    // which Prepare's R20-4 garbage gate then rejects (0 units). Comparing
    // that recomputed 1 against 0 declared hashes failed the whole `kio
    // index` with KIO-E-CONFIG-SCHEMA-001 (found on real img2pdf rasters,
    // 2026-07-23 fixture registration). The office arm always had this
    // guard; the PDF arm was the asymmetry.
    //
    // 07 §5.1 (2026-07-25): XLSX joins the same shape with a third source. Its
    // unit is a worksheet and its declared `prepared_hash` is that sheet's
    // extracted Markdown, so the bytes to store are re-derived by re-running
    // the extractor — deterministic and local, no converter involved.
    let unit_texts = if media_type == "application/pdf" && !prepared_units.is_empty() {
        Some(pdf_text_pages_bounded(bytes).map_err(pipeline_to_kio)?)
    } else if is_office_media(media_type) && !prepared_units.is_empty() {
        let converted = convert_office_to_pdf_with_control(bytes, media_type, repo.control())
            .map_err(adapter_to_kio)?;
        Some(pdf_text_pages_bounded(&converted).map_err(pipeline_to_kio)?)
    } else if kio_adapter::xlsx_extract::is_xlsx_media(media_type) && !prepared_units.is_empty() {
        Some(
            kio_adapter::xlsx_extract::extract_xlsx(bytes)
                .map_err(adapter_to_kio)?
                .sheets
                .into_iter()
                .map(|sheet| sheet.markdown)
                .collect(),
        )
    } else {
        None
    };
    if unit_texts
        .as_ref()
        .is_some_and(|texts| texts.len() != prepared_hashes.len())
    {
        return Err(KioError::schema(
            "prepared unit text and object hash cardinalities differ",
        ));
    }
    for (index, prepared_hash) in prepared_hashes.iter().enumerate() {
        if !is_hash(prepared_hash) {
            return Err(KioError::schema("prepared object hash is invalid"));
        }
        let path = cas_object_path(repo.kio_dir(), "prepared", prepared_hash)?;
        let object_bytes = unit_texts
            .as_ref()
            .and_then(|texts| texts.get(index))
            .map_or(bytes, |text| text.as_bytes());
        if hash_bytes(object_bytes) != *prepared_hash
            || prepared_units[index].prepared_hash != *prepared_hash
        {
            return Err(store_corrupt_error(
                &path,
                "prepared object bytes do not match the declared hash",
            ));
        }
        write_canonical_cas_object_or_reuse(
            repo.kio_dir(),
            "prepared",
            prepared_hash,
            object_bytes,
        )?;
    }
    Ok(())
}

fn normalized_output_ref(
    _repo: &Repository,
    raw_hash: &str,
    tool_profile_hash: &str,
    r#gen: u64,
) -> String {
    normalized_task_output_ref(raw_hash, tool_profile_hash, r#gen)
}

#[allow(clippy::too_many_arguments)]
fn task_descriptor(
    repo: &Repository,
    task_type: TaskType,
    mode: Option<MarkdownizeMode>,
    candidate: &ScanCandidate,
    input_hash: &str,
    output_ref: &str,
    status: TaskStatus,
    fallback_reason: Option<&str>,
    created_at: &str,
) -> TaskDescriptor {
    TaskDescriptor {
        task_id: format!("task_{}", new_ulid(repo.canonical_root())),
        task_type,
        mode,
        input_path: candidate.input_path.clone(),
        input_hash: input_hash.to_owned(),
        previous_raw_hash: None,
        parent_run_id: None,
        changed_unit_keys: Vec::new(),
        output_ref: output_ref.to_owned(),
        unit_keys: None,
        status,
        attempts: 0,
        next_retry_at: None,
        deadline: None,
        heartbeat_at: None,
        fallback_reason: fallback_reason.map(str::to_owned),
        created_at: created_at.to_owned(),
        bbox_annotation_enabled: None,
        // QA1 (step4b-contract-tests-p3a.md §A): derive the closed hold_reason
        // from `fallback_reason` whenever this descriptor is Paused; `None`
        // for any other status or an unrecognized reason.
        hold_reason: (status == TaskStatus::Paused)
            .then(|| fallback_reason.and_then(hold_reason_for_reason))
            .flatten(),
        reserved_usd: None,
        reserved_month: None,
        reservation_id: None,
    }
}

fn append_failed_markdownize_task(
    repo: &Repository,
    task_store: &TaskStore,
    candidate: &ScanCandidate,
    input_hash: &str,
    output_ref: &str,
    fallback_reason: &str,
    created_at: &str,
) -> Result<()> {
    let task = task_descriptor(
        repo,
        TaskType::Markdownize,
        Some(MarkdownizeMode::Full),
        candidate,
        input_hash,
        output_ref,
        TaskStatus::Failed,
        Some(fallback_reason),
        created_at,
    );
    task_store.append(&task).map_err(pipeline_to_kio)
}

fn prepared_unit_hints(prepared_units: &[PreparedUnit]) -> Vec<PreparedUnitHint> {
    prepared_units
        .iter()
        .map(|unit| PreparedUnitHint {
            unit_key: unit.unit_key.clone(),
            prepared_hash: unit.prepared_hash.clone(),
            unit_kind: adapter_unit_kind(unit.unit_type),
            order: unit.order,
        })
        .collect()
}

fn adapter_unit_kind(unit_type: UnitType) -> UnitKind {
    match unit_type {
        UnitType::Page => UnitKind::Page,
        UnitType::Slide => UnitKind::Slide,
        UnitType::HeadingSection => UnitKind::HeadingSection,
        UnitType::Sheet => UnitKind::Sheet,
        UnitType::Image => UnitKind::Image,
        UnitType::File => UnitKind::File,
        UnitType::Symbol => UnitKind::Symbol,
    }
}

fn adapter_mode(mode: MarkdownizeMode) -> AdapterMarkdownizeMode {
    match mode {
        MarkdownizeMode::Full => AdapterMarkdownizeMode::Full,
        MarkdownizeMode::Incremental => AdapterMarkdownizeMode::Incremental,
    }
}

#[derive(Debug, Clone)]
struct PreviousInstance {
    manifest: NormalizedInstanceManifest,
    units: Vec<NormalizedUnitObject>,
    prepared_units: Vec<PreparedUnit>,
}

fn previous_instance_for_path(
    task_store: &TaskStore,
    input_path: &str,
    tool_profile_hash: &str,
) -> Result<Option<PreviousInstance>> {
    let mut tasks = task_store
        .all()
        .map_err(pipeline_to_kio)?
        .into_iter()
        .filter(|task| {
            task.input_path == input_path
                && matches!(task.status, TaskStatus::Done | TaskStatus::Partial)
                && !task.output_ref.starts_with("online:")
        })
        .collect::<Vec<_>>();
    tasks.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    for task in tasks {
        let Some(previous) = load_previous_instance_for_task(task_store, &task)? else {
            continue;
        };
        if previous.manifest.tool_profile_hash == tool_profile_hash {
            return Ok(Some(previous));
        }
    }
    Ok(None)
}

fn load_previous_instance_for_task(
    task_store: &TaskStore,
    task: &TaskDescriptor,
) -> Result<Option<PreviousInstance>> {
    let identity = validate_task_output_ref(task_store.kio_dir(), task).map_err(pipeline_to_kio)?;
    let TaskOutputRef::NormalizedInstance {
        raw_hash,
        tool_profile_hash,
        r#gen,
        ..
    } = identity
    else {
        return Ok(None);
    };
    // A missing/corrupt previous instance degrades to a Full run for this document,
    // while a forged task reference is rejected above before it reaches the filesystem.
    Ok(load_previous_instance_identity(
        task_store.kio_dir(),
        &raw_hash,
        &tool_profile_hash,
        r#gen,
        task_store.control(),
    )
    .ok())
}

fn load_previous_instance_identity(
    kio_dir: &Path,
    raw_hash: &str,
    tool_profile_hash: &str,
    r#gen: u64,
    control: &ControlHandle,
) -> Result<PreviousInstance> {
    let instance = load_validated_normalized_instance_with_control(
        kio_dir,
        raw_hash,
        tool_profile_hash,
        r#gen,
        control,
    )
    .map_err(pipeline_to_kio)?;
    let manifest = instance.manifest;
    let units = instance.units;
    let prepared_units = manifest
        .units
        .iter()
        .map(|entry| PreparedUnit {
            order: entry.order,
            unit_key: entry.unit_key.clone(),
            unit_type: entry.unit_type,
            prepared_hash: entry.prepared_hash.clone(),
            preparation_profile_hash: entry.preparation_profile_hash.clone(),
            fingerprint: UnitFingerprint {
                perceptual_hash: entry.prepared_hash.clone(),
                text_hash: entry.prepared_hash.clone(),
                visual_hash: entry.prepared_hash.clone(),
            },
            mime: None,
            page_number: (entry.unit_type == UnitType::Page).then_some(entry.order + 1),
        })
        .collect();
    Ok(PreviousInstance {
        manifest,
        units,
        prepared_units,
    })
}

fn incremental_hints_from_mapping(
    mapping: &kio_pipeline::prepare::UnitMapping,
    prepared_units: &[PreparedUnit],
) -> IncrementalHints {
    IncrementalHints {
        changed_unit_keys: mapping.changed_unit_keys.clone(),
        added_unit_keys: mapping.added_unit_keys.clone(),
        removed_unit_keys: mapping.removed_unit_keys.clone(),
        page_fingerprints: prepared_units
            .iter()
            .map(|unit| (unit.unit_key.clone(), unit.fingerprint.clone()))
            .collect(),
    }
}

fn all_changed_hints(prepared_units: &[PreparedUnit]) -> IncrementalHints {
    IncrementalHints {
        changed_unit_keys: prepared_units
            .iter()
            .map(|unit| unit.unit_key.clone())
            .collect(),
        added_unit_keys: Vec::new(),
        removed_unit_keys: Vec::new(),
        page_fingerprints: prepared_units
            .iter()
            .map(|unit| (unit.unit_key.clone(), unit.fingerprint.clone()))
            .collect(),
    }
}

fn adapter_hints(hints: &IncrementalHints) -> kio_adapter::types::IncrementalHints {
    kio_adapter::types::IncrementalHints {
        changed_unit_keys: hints.changed_unit_keys.clone(),
        added_unit_keys: hints.added_unit_keys.clone(),
        removed_unit_keys: hints.removed_unit_keys.clone(),
        page_fingerprints: hints
            .page_fingerprints
            .iter()
            .map(|(key, fingerprint)| {
                (
                    key.clone(),
                    kio_adapter::types::UnitFingerprint {
                        perceptual_hash: fingerprint.perceptual_hash.clone(),
                        text_hash: fingerprint.text_hash.clone(),
                        visual_hash: fingerprint.visual_hash.clone(),
                    },
                )
            })
            .collect(),
    }
}

fn normalized_unit_to_adapter_unit(unit: &NormalizedUnitObject) -> MarkdownUnit {
    MarkdownUnit {
        unit_key: unit.unit_key.clone(),
        unit_type: adapter_unit_kind(unit.unit_type),
        markdown: unit.markdown.clone(),
        owned_image_hashes: unit.owned_image_hashes.clone(),
        metadata: unit.metadata.clone(),
    }
}

fn consecutive_incremental_count(task_store: &TaskStore, input_path: &str) -> Result<u32> {
    let mut tasks = task_store
        .all()
        .map_err(pipeline_to_kio)?
        .into_iter()
        .filter(|task| task.input_path == input_path && task.status == TaskStatus::Done)
        .collect::<Vec<_>>();
    tasks.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    let mut count = 0u32;
    for task in tasks {
        if task.mode == Some(MarkdownizeMode::Incremental) {
            count = count.saturating_add(1);
        } else {
            break;
        }
    }
    Ok(count)
}

#[derive(Debug)]
enum NormalizedUnitsError {
    Contract(KioError),
    LocalPersistence(KioError),
}

impl NormalizedUnitsError {
    fn into_kio(self) -> KioError {
        match self {
            Self::Contract(error) | Self::LocalPersistence(error) => error,
        }
    }

    fn task_failure(self) -> TaskExecutionFailure {
        let retry_kind = match self {
            Self::Contract(_) => RetryErrorKind::ContractViolation,
            // The provider response has already been accepted. CAS write/inspect
            // failures are local recovery faults and batch collection must retain
            // its known result instead of submitting a paid request again.
            Self::LocalPersistence(_) => persist_failure_retry_kind(),
        };
        TaskExecutionFailure {
            retry_kind,
            retry_after_ms: None,
        }
    }
}

impl From<KioError> for NormalizedUnitsError {
    fn from(error: KioError) -> Self {
        Self::Contract(error)
    }
}

impl From<image_ingest::SourceImageRetentionError> for NormalizedUnitsError {
    fn from(error: image_ingest::SourceImageRetentionError) -> Self {
        match error {
            image_ingest::SourceImageRetentionError::SourceValidation(error) => {
                Self::Contract(error)
            }
            image_ingest::SourceImageRetentionError::LocalPersistence(error) => {
                Self::LocalPersistence(error)
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn normalized_units_from_response(
    repo: &Repository,
    source_media_type: &str,
    verified_raw_bytes: &[u8],
    response: &kio_adapter::types::MarkdownizeResponse,
    prepared_units: &[PreparedUnit],
    previous: Option<&PreviousInstance>,
    raw_hash: &str,
    tool_profile_hash: &str,
    // QB41 (03 §2.1's second legal gen+1 path, 07 §5.1): every EXISTING call
    // site passes `0` (first-instance-wins: a fresh `(raw_hash,
    // tool_profile_hash)` identity always starts at gen 0). The ONLY caller
    // that passes non-zero is `create_prepared_hash_drift_instance` -- a
    // prepared_hash drift under an UNCHANGED raw_hash is the second gen+1
    // path this field newly makes representable.
    r#gen: u64,
    mode: MarkdownizeMode,
    generated_at: &str,
) -> std::result::Result<Vec<NormalizedUnitObject>, NormalizedUnitsError> {
    let prepared = prepared_units
        .iter()
        .map(|unit| (unit.unit_key.as_str(), unit))
        .collect::<BTreeMap<_, _>>();
    let previous_units = previous
        .map(|previous| {
            previous
                .units
                .iter()
                .map(|unit| (unit.unit_key.as_str(), unit))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let mut units = response
        .updated_units
        .iter()
        .chain(response.added_units.iter())
        .map(|unit| {
            let prepared = prepared
                .get(unit.unit_key.as_str())
                .ok_or_else(|| KioError::schema("adapter returned unknown unit"))?;
            let markdown = image_ingest::retain_original_source_image_reference(
                repo,
                raw_hash,
                verified_raw_bytes,
                source_media_type,
                prepared.unit_type,
                &unit.markdown,
            )?;
            let mut owned_image_hashes = unit.owned_image_hashes.clone();
            // A standalone image's raw identity was verified against these
            // exact bytes before source retention created its image CAS object.
            // It is therefore a typed owner even when a text-only OCR response
            // did not return its own image list.
            if prepared.unit_type == UnitType::Image {
                owned_image_hashes.insert(raw_hash.to_owned());
            }
            Ok(NormalizedUnitObject {
                unit_key: unit.unit_key.clone(),
                unit_type: prepared.unit_type,
                raw_hash: raw_hash.to_owned(),
                prepared_hash: prepared.prepared_hash.clone(),
                preparation_profile_hash: prepared.preparation_profile_hash.clone(),
                tool_profile_hash: tool_profile_hash.to_owned(),
                r#gen,
                mode,
                markdown,
                owned_image_hashes,
                metadata: unit.metadata.clone(),
                reused_from: None,
                generated_at: generated_at.to_owned(),
            })
        })
        .collect::<std::result::Result<Vec<_>, NormalizedUnitsError>>()?;
    for unit_key in &response.unchanged_unit_keys {
        let prepared = prepared
            .get(unit_key.as_str())
            .ok_or_else(|| KioError::schema("adapter returned unknown unchanged unit"))?;
        let previous_unit = previous_units
            .get(unit_key.as_str())
            .ok_or_else(|| KioError::schema("unchanged unit has no previous normalized unit"))?;
        units.push(NormalizedUnitObject {
            unit_key: unit_key.clone(),
            unit_type: prepared.unit_type,
            raw_hash: raw_hash.to_owned(),
            prepared_hash: prepared.prepared_hash.clone(),
            preparation_profile_hash: prepared.preparation_profile_hash.clone(),
            tool_profile_hash: tool_profile_hash.to_owned(),
            r#gen,
            mode,
            // `unchanged_unit_keys` denotes a pinned unit from the prior
            // normalized instance. Reuse must preserve its immutable Markdown
            // verbatim; source-image retention applies only when an adapter
            // emits a fresh or updated image unit.
            markdown: previous_unit.markdown.clone(),
            // Unchanged reuse is immutable: preserve the prior typed source
            // ownership exactly, rather than reinterpreting retained Markdown.
            owned_image_hashes: previous_unit.owned_image_hashes.clone(),
            metadata: previous_unit.metadata.clone(),
            reused_from: Some(kio_pipeline::markdownize::ReusedFrom {
                raw_hash: previous_unit.raw_hash.clone(),
                r#gen: previous_unit.r#gen,
                unit_key: previous_unit.unit_key.clone(),
            }),
            generated_at: generated_at.to_owned(),
        });
    }
    units.sort_by_key(|unit| {
        prepared
            .get(unit.unit_key.as_str())
            .map(|unit| unit.order)
            .unwrap_or(u64::MAX)
    });
    Ok(units)
}

/// Convert a response's already-validated per-unit failures into the immutable
/// manifest's retry vocabulary. `validate_markdownize_response` admits only the
/// closed `RetryErrorKind` spelling before this boundary; the fallback remains a
/// defensive invariant for callers that construct a manifest without a response.
fn failed_unit_retry_kinds(
    response: &kio_adapter::types::MarkdownizeResponse,
) -> BTreeMap<String, RetryErrorKind> {
    response
        .failed_units
        .iter()
        .map(|failed| {
            (
                failed.unit_key.clone(),
                retry_kind_from_reason(Some(&failed.error_kind)),
            )
        })
        .collect()
}

/// Rebuilding a retry subset still writes the full prepared-unit manifest. Keep
/// the prior immutable failure kind for every unit not requested this time, then
/// let the accepted provider response replace kinds for the requested subset.
fn merged_failed_unit_retry_kinds(
    response: &kio_adapter::types::MarkdownizeResponse,
    previous: Option<&PreviousInstance>,
    retry_units: Option<&BTreeSet<String>>,
) -> BTreeMap<String, RetryErrorKind> {
    let mut kinds = BTreeMap::new();
    if let (Some(previous), Some(retry_units)) = (previous, retry_units) {
        for entry in &previous.manifest.units {
            if entry.status == UnitStatus::Failed && !retry_units.contains(&entry.unit_key) {
                kinds.insert(
                    entry.unit_key.clone(),
                    retry_kind_from_reason(entry.error_kind.as_deref()),
                );
            }
        }
    }
    kinds.extend(failed_unit_retry_kinds(response));
    kinds
}

#[allow(clippy::too_many_arguments)]
fn manifest_from_units(
    prepared_units: &[PreparedUnit],
    units: &[NormalizedUnitObject],
    raw_hash: &str,
    tool_profile_hash: &str,
    r#gen: u64,
    parent_gen: Option<u64>,
    run_id: &str,
    generated_at: &str,
    failed_kind: RetryErrorKind,
) -> NormalizedInstanceManifest {
    manifest_from_units_with_failed_kinds(
        prepared_units,
        units,
        raw_hash,
        tool_profile_hash,
        r#gen,
        parent_gen,
        run_id,
        generated_at,
        &BTreeMap::new(),
        failed_kind,
    )
}

#[allow(clippy::too_many_arguments)]
fn manifest_from_units_with_failed_kinds(
    prepared_units: &[PreparedUnit],
    units: &[NormalizedUnitObject],
    raw_hash: &str,
    tool_profile_hash: &str,
    // QB41 (03 §2.1's second legal gen+1 path): see `normalized_units_from_response`'s
    // matching `gen` doc -- every existing call site passes `0`.
    r#gen: u64,
    parent_gen: Option<u64>,
    run_id: &str,
    generated_at: &str,
    failed_kinds: &BTreeMap<String, RetryErrorKind>,
    fallback_failed_kind: RetryErrorKind,
) -> NormalizedInstanceManifest {
    let done = units
        .iter()
        .map(|unit| unit.unit_key.as_str())
        .collect::<BTreeSet<_>>();
    NormalizedInstanceManifest {
        raw_hash: raw_hash.to_owned(),
        tool_profile_hash: tool_profile_hash.to_owned(),
        r#gen,
        parent_gen,
        run_id: run_id.to_owned(),
        units: prepared_units
            .iter()
            .map(|unit| NormalizedUnitManifestEntry {
                order: unit.order,
                unit_key: unit.unit_key.clone(),
                unit_ref: unit_ref(&unit.unit_key),
                unit_type: unit.unit_type,
                status: if done.contains(unit.unit_key.as_str()) {
                    UnitStatus::Done
                } else {
                    UnitStatus::Failed
                },
                prepared_hash: unit.prepared_hash.clone(),
                preparation_profile_hash: unit.preparation_profile_hash.clone(),
                unit_object_hash: None,
                // Preserve the provider's admitted per-unit outcome. A fallback
                // only serves internal callers that have no response object.
                error_kind: (!done.contains(unit.unit_key.as_str())).then(|| {
                    retry_reason(
                        failed_kinds
                            .get(&unit.unit_key)
                            .copied()
                            .unwrap_or(fallback_failed_kind),
                    )
                    .to_owned()
                }),
            })
            .collect(),
        generated_at: generated_at.to_owned(),
    }
}

/// R19-3: terminal `fallback_reason` for a task retired because its content-addressed
/// target (chunk_id or (path,hash)) is currently NON-LIVE. Unlike `invalid_input`
/// (a permanent, genuinely-bad-input failure), this retirement is REVERSIBLE: the
/// identity can reappear (git revert / undo / restore of the exact bytes), so the
/// `enqueue_*` idempotency guards treat it as non-blocking and let a fresh task be
/// created. It is still non-retryable AS THE OLD TASK (`retry_kind_from_reason` maps it
/// to InvalidInput), so it is never re-driven and never counts as pending work.
const RETIRED_NON_LIVE: &str = "retired_non_live";

// ---------------------------------------------------------------------------
// cost-ledger.sqlite task-charge helpers (04-pipeline.md §5.4's sync degenerate
// 2-phase — CL41-47/CL56-61 in tasks/step4b-contract-tests-ledger.md). Replaces
// the retired JSONL `budget::CostLedger`/`ReservationLedger` F8 reservation flow
// (2026-07-21) — see the implementation report for the behavior changes this
// entails (a "markdownize"/"embedding" TASK's online adapter call is now itself
// one `batch_requests` sync row per §G, instead of a JSONL charge plus a
// side-channel reservation-lifecycle ledger).
// ---------------------------------------------------------------------------

/// Effective `timeout_seconds` this session applies to a markdownize/embedding
/// TASK sync row's `stale_after_at` (CL49's formula, 07-adapter-spec.md §7's
/// documented `[adapter.policy]` default). This implementation's device
/// query-embedding row (§H, `compute_query_embedding`) resolves the effective
/// timeout per §M note-2's config-layer rule; TASK rows use this fixed default
/// instead — the same "conservative fixed constant, not a live adapter-config
/// re-read" posture `estimate_online_markdownize_cost`/`estimate_embedding_cost`
/// already use for the *amount* side of the charge. Flagged in the
/// implementation report as unfinished per-scope timeout resolution for task rows.
const TASK_SYNC_EFFECTIVE_TIMEOUT_SECONDS: i64 = 300;

/// One markdownize/embedding TASK's `cost-ledger.sqlite` row identity — every
/// online adapter call this module drives (as opposed to the device
/// query-embedding row, §H) is a `request_kind='sync'` row under this key (04
/// §5.4 L768 / CL42). `adapter_kind` must be one of
/// `kio_pipeline::ledger::ops::PER_ADAPTER_KIND_ENUM` (CL61) — this module uses
/// the literals `"markdownize"` / `EMBEDDING_ADAPTER_KIND` directly rather than
/// the retired `adapter_kind_budget_key` helper, whose
/// `AdapterKind::Markdownize => "markdown"` mapping was the retired JSONL
/// ledger's key spelling (the unrelated tool-lock JSON schema hardcodes its own
/// literal `"markdown"` key directly in `materialize_tool_lock` and never used
/// that helper — CL61's "markdown"→"markdownize" rename is scoped to the
/// budget/ledger namespace only).
fn task_ledger_key(
    scope_id: &str,
    adapter_kind: &'static str,
    input_hash: &str,
    tool_profile_hash: &str,
) -> LedgerTaskKey {
    LedgerTaskKey::new(scope_id, adapter_kind, input_hash, tool_profile_hash)
}

/// CL57's third condition / CL61: `[budget.per_adapter]`'s device-layer-only cap
/// for `adapter_kind` (`caps.device_per_adapter`'s keys are already validated
/// against the closed enum by `budget::read_budget_config`).
fn per_adapter_cap_for(caps: &BudgetCaps, adapter_kind: &str) -> Option<f64> {
    caps.device_per_adapter.get(adapter_kind).copied()
}

#[must_use]
fn budget_cap_config(
    caps: &BudgetCaps,
    scope_id: &str,
    adapter_kind: &'static str,
) -> BudgetCapConfig {
    let _ = scope_id; // folder cap is scope-independent in shape (CL56); kept for call-site symmetry
    BudgetCapConfig {
        device_cap: caps.device_monthly_usd_cap,
        folder_cap: caps.folder_monthly_usd_cap,
        device_per_adapter_cap: per_adapter_cap_for(caps, adapter_kind),
    }
}

/// Outcome of [`reserve_or_reuse_task_charge`].
enum TaskChargeOutcome {
    /// A brand-new phase 1 (or the `candidate_usd == 0` exemption, CL58) landed.
    Reserved {
        intent_token: String,
        estimated_usd: f64,
    },
    /// An earlier attempt's reservation is still open and covers this resend
    /// (CL42/CL44) — the ledger was not touched.
    Reused {
        intent_token: String,
        estimated_usd: f64,
    },
    /// The device/folder/per_adapter cap would be exceeded (CL56-58).
    BudgetExceeded,
}

/// CL41-44/CL56-61: reserve (phase 1, check-then-reserve, atomic with the cap
/// check) — or, if a live (non-terminal) row already exists for `key`, reuse its
/// `intent_token` without any ledger write at all. This single check replaces the
/// retired JSONL design's task-side `fallback_reason`-driven "RateLimit/
/// QuotaExceeded resend reuses the prior reservation" special case (formerly
/// R16-7): under the SQLite ledger, a non-terminal row IS the live reservation —
/// `phase1_intent`'s own cleanup guard refuses a second phase 1 while one is open
/// (04 §5.8 順序規範) — so "does an open row already exist for this task key" is
/// the whole rule, independent of which retryable error kind left it open.
///
/// `bypass_cap_denial` folds together the two documented reasons a Denied cap
/// check must still proceed to a real reservation (docs/04 §5.4):
/// `kio batch resume --override-budget` (applied "symmetrically" to
/// markdownize/embedding — a one-shot, per-invocation override), and
/// `[budget] hard_stop = false` (F5's persistent soft-stop config: "record the
/// charge and continue over cap" instead of pausing). Both are applied HERE at
/// the call site rather than inside `ops::check_then_reserve` itself: that
/// function's own `BudgetCapConfig`/CL56-61 contract tests pin an
/// unconditional check-then-reserve with no override or hard_stop parameter of
/// its own (this session's read of `kio_pipeline::ledger::ops`'s existing,
/// already-contract-tested API surface — extending it is out of this item's
/// scope, flagged in the report), so a Denied result under either bypass
/// condition falls through to the same `phase1_intent` the Allowed/
/// ExemptZeroCost arms use, skipping only the cap comparison — the
/// reservation itself, and everything downstream of it (settlement,
/// `intent_token`, `stale_after_at`), is unchanged.
fn reserve_or_reuse_task_charge(
    ctx: &Invocation<'_>,
    ledger: &LedgerDb,
    key: &LedgerTaskKey,
    candidate_usd: f64,
    caps: &BudgetCapConfig,
    bypass_cap_denial: bool,
    // Batch-lane wiring (04 §5.8 相 1): the fresh reservation's
    // `batch_requests.request_kind`. Every pre-existing caller passes
    // `RequestKind::Sync` (behavior unchanged — `stale_after_at` computed from
    // the sync effective timeout exactly as before); the Batch send lane
    // passes `RequestKind::Batch`, for which `phase1_intent` records no
    // `stale_after_at`/`job_create_started_at` (§5.8's own recovery-deadline
    // mechanism covers batch rows instead — see `ops::phase1_intent`'s doc).
    request_kind: RequestKind,
) -> Result<TaskChargeOutcome> {
    // QA67 (step4b-contract-tests-p3a.md §T, PB24): online task phase 1 must
    // fail-closed on a live registry scope_id duplicate (10 §3 L297-299) —
    // two `.kio` clones sharing this device-global `batch_requests` row's PK
    // (scope_id, adapter_kind, input_hash, tool_profile_hash) mix up which
    // clone owns the reservation's recovery/settlement/billing attribution.
    registry_duplicate_guard(ctx, &key.scope_id)?;
    let sync_timeout =
        (request_kind == RequestKind::Sync).then_some(TASK_SYNC_EFFECTIVE_TIMEOUT_SECONDS);
    use kio_pipeline::ledger::ops::ReserveOrReuseOutcome;
    let outcome = kio_pipeline::ledger::ops::reserve_or_reuse(
        ledger,
        key,
        candidate_usd,
        caps,
        request_kind,
        sync_timeout,
        bypass_cap_denial,
    )
    .map_err(pipeline_to_kio)?;
    Ok(match outcome {
        ReserveOrReuseOutcome::Reused {
            intent_token,
            estimated_usd,
        } => TaskChargeOutcome::Reused {
            intent_token,
            estimated_usd,
        },
        ReserveOrReuseOutcome::Reserved {
            intent_token,
            estimated_usd,
        } => TaskChargeOutcome::Reserved {
            intent_token,
            estimated_usd,
        },
        ReserveOrReuseOutcome::BudgetExceeded => TaskChargeOutcome::BudgetExceeded,
    })
}

/// QA18/QA19 (step4b-contract-tests-p3a.md §F): `AdapterProfile.billable_kinds`
/// (the typed `BillableUnitKind` enum) as the string keys
/// `resolve_billing_from_reported_usage`'s `declared_billable_kinds` parameter
/// expects (`kio-pipeline`'s ledger, which has no dependency on the specific
/// adapter-profile-construction code in this crate, keys billing resolution
/// off plain strings — see `ledger::ops::BILLABLE_UNIT_KINDS`).
fn declared_billable_kind_strings(profile: &AdapterProfile) -> std::collections::BTreeSet<String> {
    profile
        .billable_kinds
        .iter()
        .map(|kind| match kind {
            kio_adapter::types::BillableUnitKind::Pages => "pages",
            kio_adapter::types::BillableUnitKind::TokensIn => "tokens_in",
            kio_adapter::types::BillableUnitKind::TokensOut => "tokens_out",
        })
        .map(str::to_owned)
        .collect()
}

/// QA19 (step4b-contract-tests-p3a.md §F, 07 §4 L266-269): record a
/// one-per-run `level=warn` when a `billable_units[].kind` the Adapter
/// reported has no `tools.toml` `[pricing]` entry ("単価未被覆の kind"). The
/// settle call site already degrades that charge to `estimated`
/// (`resolve_billing_from_reported_usage` itself, unconditionally) — this
/// only makes the pricing gap observable instead of a silent under/over-bill.
/// Deduped per `(adapter_kind, sorted kinds)` for the whole process, mirroring
/// the adapter-activation warning path. A no-op for an empty `kinds` (the common
/// case: fully-covered or no billable_units report at all).
fn warn_uncovered_pricing_kinds_once(
    adapter_kind: &str,
    kinds: &[String],
    control: &ControlHandle,
    session: &kio_core::scope::LockSession,
) {
    if kinds.is_empty() {
        return;
    }
    use std::sync::{Mutex, OnceLock};
    static WARNED: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    let warned = WARNED.get_or_init(|| Mutex::new(std::collections::HashSet::new()));
    let dedup_key = format!("{adapter_kind}:{}", kinds.join(","));
    let mut guard = warned
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !guard.insert(dedup_key) {
        return;
    }
    drop(guard);
    let _ = control.append_warn_log_in_session(
        session,
        "KIO-W-ADAPTER-PRICING-UNCOVERED-001",
        "billable_units reported a kind with no tools.toml [pricing] entry; \
         billing degraded to estimated",
        json!({ "adapter_kind": adapter_kind, "kinds": kinds }),
    );
}

/// CL18/CL26/CL47: settle a task's sync row as a successful terminal charge,
/// billed at `billed` — the caller resolves this via
/// `resolve_billing_from_reported_usage` (QA17/QA18/QA19), which degrades to
/// `estimated=1` at the reservation estimate whenever the Adapter has no real
/// usage to report (this codebase's Adapters mostly don't yet —
/// 07-adapter-spec.md's Batch trait does not exist here yet — confirmed by
/// grep, `ledger::ops`'s own module doc; Mistral OCR's markdownize response
/// is the one exception with a real page-count self-report). `intent_token`
/// clears in the same Tx — sync rows always clear immediately on ANY terminal
/// Tx, unlike batch rows (CL47).
///
/// R23-03 (04 §5.4: "device 行の全ての状態遷移 UPDATE ... は `WHERE
/// intent_token = <自 token>` の条件付き (CAS) で行う"): CAS-guarded on our own
/// `intent_token`. This function is shared by both scope-keyed task charges
/// (serialized by `.kio/.lock` — the guard is a harmless no-op there, since no
/// concurrent writer can have changed the row) and the device query-embedding
/// row (§H — NOT lock-serialized; a concurrent stale-sweep recoverer can
/// reclaim this exact row under a NEW token while this call's own adapter
/// call is still in flight). A 0-row CAS miss means our claim was already
/// reclaimed — record nothing (the reclaimer's own `unknown_settled` already
/// covers this attempt); the vector this call already received from the
/// adapter is still returned to the caller for THIS search's own result,
/// independent of billing (same 04 §5.4 clause: "受信済み vector を当該検索の
/// 結果に使うことは課金と独立に可").
fn settle_task_charge_success(
    ledger: &LedgerDb,
    key: &LedgerTaskKey,
    intent_token: &str,
    billed: BilledAmount,
) -> Result<()> {
    terminal_transaction(
        ledger,
        &TerminalWrite {
            key,
            outcome: Outcome::Succeeded,
            billed,
            // DDL's 3-way `batch_job_id` rule (04 §5.4): a sync call with no
            // provider request id falls back to the attempt's own intent_token.
            ledger_batch_job_id: intent_token,
            next_state: BatchState::Completed,
            error: None,
            increment_contract_violation: false,
            attempts_delta: 1,
            clear_intent_token: true,
            intent_token_guard: Some(intent_token),
            reseat_submission_seq: false,
        },
    )
    .map_err(pipeline_to_kio)
    .map(|_| ())
}

/// CL45's "cannot confirm the outcome" settlement (`outcome='unknown_settled'`,
/// billed at the conservative reservation estimate — "over-count safer than
/// under-count"). Used both for a task's definitively-failed send (retry budget
/// exhausted, or a non-retryable error kind) and for releasing a task's charge
/// without ever resending (superseded by an edit, deleted, purged, demoted to a
/// secrets hold, …). This codebase's Adapters have no post-hoc result-query
/// capability at all (CL45: an Adapter without one "常に...unknown 精算になる"),
/// so `unknown_settled` is this module's uniform non-success terminal outcome —
/// see the implementation report for how this differs from the retired JSONL
/// design's surgical reclaim-to-zero of specifically-non-billable error kinds
/// (RateLimit/QuotaExceeded/AuthError): every other terminal outcome in the
/// closed 8-value enum (CL26) either requires data this module never has
/// (`contract_violation`/`purged`/`submit_rejected`/`fallback_to_full` all
/// presuppose an Adapter-reported structured signal this sync integration does
/// not produce) or is a distinct CLI-driven action (`abandoned`, CL64).
fn settle_task_charge_unknown(
    ledger: &LedgerDb,
    key: &LedgerTaskKey,
    intent_token: &str,
    estimated_usd: f64,
) -> Result<()> {
    recovery_settle_unknown(ledger, key, intent_token, estimated_usd, true)
        .map_err(pipeline_to_kio)
        .map(|_| ())
}

/// If a live (non-terminal) sync row exists for `key`, settle it unknown
/// (`settle_task_charge_unknown`). A no-op returning `false` when no such row
/// exists (already terminal-and-cleaned, or never reserved) — idempotent,
/// mirroring CL66's "target-less op is a success" posture. Used by every
/// "release this task's charge without resending" call site (supersede,
/// purge-adjacent demotes, auth-revive, non-live retirement, …) that the retired
/// JSONL design drove through `settle_task_reservation`.
///
/// Batch-lane rows (04 §5.8): a `request_kind='batch'` row that reached phase
/// 2a (`provider_scope_id` recorded) may hold provider residue (upload/job)
/// whose ONLY discovery key is the filename-embedded `intent_token` — the
/// settlement here must NOT clear the token in the same Tx (cleanup-first;
/// same `provider_scope_id IS NULL` rule as `execute_abandon`'s note-5).
/// Reconcile/abandon complete the residue cleanup and the eventual NULL-out.
fn release_task_charge_if_open(ledger: &LedgerDb, key: &LedgerTaskKey) -> Result<bool> {
    let Some(row) = get_batch_request(ledger, key).map_err(pipeline_to_kio)? else {
        return Ok(false);
    };
    if !row.state.is_inflight() {
        return Ok(false);
    }
    let Some(intent_token) = row.intent_token.clone() else {
        return Ok(false);
    };
    let cleanup_already_complete =
        row.request_kind != RequestKind::Batch || row.provider_scope_id.is_none();
    recovery_settle_unknown(
        ledger,
        key,
        &intent_token,
        row.estimated_usd,
        cleanup_already_complete,
    )
    .map_err(pipeline_to_kio)?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Mistral OCR Batch lane (04 §5.8 / 07 §5.5): submit + poll/collect
// ---------------------------------------------------------------------------

/// Poll cadence for an in-flight batch job (task `next_retry_at` extension per
/// submit/poll pass).
const BATCH_POLL_INTERVAL_MS: u64 = 60_000;

/// `fallback_reason` stamped on a Pending task whose send this pass took the
/// Batch lane (job created, awaiting collect). Deliberately NOT one of the
/// retry-failure reasons: `retry_kind_from_reason` maps it to its
/// ContractViolation default, but no code path reads a PENDING task's reason
/// as a retry kind (`task_retry_allowed` is Failed-only), and
/// `is_reservation_bearing_send_failure`'s sweep set does not include it — an
/// in-flight batch task must never be retired as a stale send failure while
/// its provider job is live.
const BATCH_SUBMITTED_REASON: &str = "batch_submitted";

/// The configured Batch client, if the Batch lane is available at all.
///
/// Lane availability deliberately equals "`configured_mistral_batch_client()`
/// returned a client". 07 §5.5 frames the lane choice as the adapter's
/// `preferred_request_kind()` declaration AND a configured client, but the
/// concrete `MistralOcrMarkdownizeAdapter` type is module-private to
/// `kio-adapter` (no catalog constructor returns it as a `dyn
/// MarkdownizeAdapter`), so this crate cannot instantiate one to ask. The
/// frozen resolver embodies the declaration instead: it returns `Some`
/// exactly when the hermetic mock seam (`KIO_TEST_MISTRAL_BATCH`) is present
/// — the test path — or (once the adapter-side env client lands) when the
/// Batch-declaring Mistral profile is configured for production.
fn batch_markdownize_lane_client(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    ledger: &LedgerDb,
) -> Result<Option<Box<dyn MistralBatchClient>>> {
    configured_mistral_batch_client_with_control(
        adapter_settings,
        || batch_recovery_context(ledger),
        ledger.control(),
    )
    .map_err(adapter_to_kio)
}

/// Check only the locally declared sync credential. This never constructs an
/// HTTP client or contacts the provider. Test adapters keep their established
/// failure semantics because their mock execution can model a post-admission
/// auth failure rather than an absent production credential.
fn sync_markdownize_credential_available(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    _control: &ControlHandle,
) -> Result<bool> {
    #[cfg(debug_assertions)]
    if markdownize_test_credential_available(_control.debug()) {
        return Ok(true);
    }
    Ok(adapter_settings
        .resolve_role_api_key("markdown")
        .map_err(adapter_to_kio)?
        .is_some_and(|credential| !credential.trim().is_empty()))
}

#[cfg(debug_assertions)]
fn markdownize_test_credential_available(control: &DebugTestControl) -> bool {
    control.cli.markdownize_adapter.known().is_some()
        || matches!(
            control.adapters.mistral_ocr,
            kio_core::test_control::Selector::Known(_)
        )
}

/// Which lane one Pending online-markdownize task's send takes this pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkdownizeSendLane {
    Sync,
    Batch,
    /// An open `request_kind='batch'` ledger row exists but no batch client is
    /// configured this pass (e.g. the mock seam vanished): neither lane can
    /// act without either racing §5.8 recovery or hiding provider residue, so
    /// the task is left untouched (counted in-flight) for reconcile/abandon.
    DeferToRecovery,
}

fn markdownize_send_lane(
    batch_client_present: bool,
    open_row: Option<&BatchRequestRow>,
    realtime: bool,
) -> MarkdownizeSendLane {
    let open_row_kind = open_row
        .filter(|row| row.state.is_inflight())
        .map(|row| row.request_kind);
    match (batch_client_present, open_row_kind) {
        // A prior synchronous request may already have reached a non-idempotent
        // provider. It is recover-only; never reuse its reservation to resend.
        (_, Some(RequestKind::Sync)) => MarkdownizeSendLane::DeferToRecovery,
        // An open batch row owns its provider job regardless of `--realtime`:
        // switching lanes mid-flight would strand that job outside §5.8
        // recovery. Realtime only chooses the lane for a NEW send.
        (false, Some(RequestKind::Batch)) => MarkdownizeSendLane::DeferToRecovery,
        (true, Some(RequestKind::Batch)) => MarkdownizeSendLane::Batch,
        (false, None) => MarkdownizeSendLane::Sync,
        // 2026-07-23 ruling: OCR spending is batch-only, WITHOUT a bbox
        // carve-out — and bbox annotation defaults to ON
        // (`effective_bbox_annotation_policy`), so a bbox exception here
        // would silently keep production on the sync lane. `ocr_batch_body`
        // carries `bbox_annotation_format` exactly like the sync request.
        //
        // 2026-07-24 ruling AMENDS that: `--realtime` is an explicit, opt-in
        // request for immediate turnaround that accepts the sync lane's double
        // rate ($4 vs $2 per 1,000 pages), and it moves OCR and embedding
        // together — never one lane each.
        (true, None) if realtime => MarkdownizeSendLane::Sync,
        (true, None) => MarkdownizeSendLane::Batch,
    }
}

/// The model string a batch job is created with (`create_job`'s second
/// argument — the model rides the JOB, not the input line, 07 §5.5).
fn batch_job_model(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    control: &ControlHandle,
) -> String {
    let declared = adapter_settings
        .declared_adapter("markdown")
        .and_then(|declared| declared.model)
        .unwrap_or_else(|| "mistral-ocr-latest".to_owned());
    if !declared.ends_with("-latest") {
        return declared;
    }
    // A mutable alias must be resolved to a versioned pin before the call
    // (07 §6). The alias resolver lives on the module-private sync OCR
    // client, unreachable from this crate; under the hermetic mock seam use
    // the same pin the KIO_TEST_MISTRAL_OCR mock resolves to so tests stay
    // network-free and profile-consistent. The real lane's alias resolution
    // is expected from the adapter-side EnvMistralBatchClient (in progress);
    // until it lands `configured_mistral_batch_client()` returns None in
    // production, so the alias fallback below can only reach a mock.
    if mistral_batch_mock_enabled(control) {
        "mistral-ocr-2505".to_owned()
    } else {
        declared
    }
}

#[cfg(debug_assertions)]
fn mistral_batch_mock_enabled(control: &ControlHandle) -> bool {
    matches!(
        control.debug().adapters.mistral_batch,
        kio_core::test_control::Selector::Known(_)
    )
}

#[cfg(not(debug_assertions))]
fn mistral_batch_mock_enabled(_control: &ControlHandle) -> bool {
    false
}

/// `next_retry_at` for the next batch poll (`now + 60s`, KIO_FIXED_NOW-aware
/// via `now_utc_seconds`).
fn batch_poll_retry_at(control: &ControlHandle) -> String {
    rate_limit_retry_at(&control.now_utc_seconds(), Some(BATCH_POLL_INTERVAL_MS))
}

/// Submit one online-markdownize task on the Batch lane (04 §5.8 手順 2-3).
///
/// The ledger write order is the §5.8 norm exactly: `provider_scope_id`
/// immediately BEFORE the upload → `upload_id` immediately after → a
/// standalone durable `job_create_started_at` → `create_job` →
/// `batch_job_id` + state=1. A crash (or provider error) between any two
/// steps leaves a row the §5.8 recovery walk can reconcile — that is why a
/// mid-submit adapter error returns the failure WITHOUT rolling any of those
/// records back.
///
/// Outer `Err` = local store/ledger fault (propagates); inner `Err` = the
/// adapter-boundary failure to feed the shared send-failure mapping.
#[allow(clippy::too_many_arguments)]
fn submit_batch_markdownize_job(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    client: &dyn MistralBatchClient,
    ledger: &LedgerDb,
    key: &LedgerTaskKey,
    intent_token: &str,
    task: &TaskDescriptor,
    prepared_input: &PreparedOnlineMarkdownizeInput,
) -> Result<std::result::Result<(), TaskExecutionFailure>> {
    let conn = &ledger;
    let Some(row) = get_batch_request(conn, key).map_err(pipeline_to_kio)? else {
        return Err(KioError::schema(
            "batch submit reached without a phase-1 batch_requests row",
        ));
    };
    if row.intent_token.as_deref() != Some(intent_token) {
        // Claim superseded between reserve and submit — nothing was sent.
        return Ok(Ok(()));
    }
    if row.batch_job_id.is_some() {
        // Already created (a reused in-flight attempt) — the poll lane owns it.
        return Ok(Ok(()));
    }
    if row.provider_scope_id.is_some() {
        // Phase 2a already started under THIS token by an earlier crashed
        // pass. Re-uploading would duplicate provider residue under one
        // token; §5.8 recovery (upload matching / reconcile) owns the row
        // until it is cleaned or self-described. Conservative: skip.
        return Ok(Ok(()));
    }
    // Same input material as the sync lane (07 §5.1): Office media is sent as
    // its converted PDF; storage identity stays the ORIGINAL file's raw_hash.
    let (effective_media_type, send_bytes): (String, Vec<u8>) =
        if is_office_media(&prepared_input.media_type) {
            match convert_office_to_pdf_with_control(
                &prepared_input.bytes,
                &prepared_input.media_type,
                ledger.control(),
            ) {
                Ok(converted) => ("application/pdf".to_owned(), converted),
                Err(error) => return Ok(Err(task_failure_from_adapter(error))),
            }
        } else {
            (
                prepared_input.media_type.clone(),
                prepared_input.bytes.clone(),
            )
        };
    // A unit-scoped retry sends only the still-failed subset's pages, exactly
    // like the sync lane's `restrict_to_hint_pages` (R15-5); a full send (or
    // OCR-from-scratch discovery, empty prepared units) sends no `pages`.
    let retry_units: Option<BTreeSet<String>> = task
        .unit_keys
        .as_ref()
        .map(|keys| keys.iter().cloned().collect());
    let pages: Option<Vec<u32>> = match &retry_units {
        Some(keys) => {
            let mut orders = Vec::new();
            for unit in prepared_input
                .prepared_units
                .iter()
                .filter(|unit| keys.contains(&unit.unit_key))
            {
                let Ok(order) = u32::try_from(unit.order) else {
                    return Ok(Err(TaskExecutionFailure {
                        retry_kind: RetryErrorKind::InvalidInput,
                        retry_after_ms: None,
                    }));
                };
                orders.push(order);
            }
            if orders.is_empty() {
                // The precondition check already validated the subset against
                // the prepared units; an empty projection means the task is
                // stale — never send an unbounded full-document job for it.
                return Ok(Err(TaskExecutionFailure {
                    retry_kind: RetryErrorKind::InvalidInput,
                    retry_after_ms: None,
                }));
            }
            Some(orders)
        }
        None => None,
    };
    let body = ocr_batch_body(
        &effective_media_type,
        &send_bytes,
        pages.as_deref(),
        task.bbox_annotation_enabled.unwrap_or(false),
    );
    // custom_id = the task's FULL input_hash (帰属 key for output-line
    // matching — 04 §5.8; the ledger row + job metadata carry the rest).
    let mut jsonl = batch_input_line(&task.input_hash, &body).into_bytes();
    jsonl.push(b'\n');
    let filename = batch_upload_filename(intent_token);
    // The 5-key metadata contract shared with the adapter-side inventory
    // (10 §7.5.2 帰属規範): intent_token + the 4-tuple task key, nothing else.
    let metadata = json!({
        "intent_token": intent_token,
        "scope_id": key.scope_id,
        "adapter_kind": key.adapter_kind,
        "input_hash": key.input_hash,
        "tool_profile_hash": key.tool_profile_hash,
    });
    let provider_scope_id = match client.provider_scope_id() {
        Ok(value) => value,
        Err(error) => return Ok(Err(task_failure_from_adapter(error))),
    };
    if !phase2a_record_provider_scope(conn, key, intent_token, &provider_scope_id)
        .map_err(pipeline_to_kio)?
    {
        return Ok(Ok(())); // CAS miss: claim superseded — nothing sent.
    }
    let upload_id = match client.upload_batch_input(&jsonl, &filename) {
        Ok(value) => value,
        Err(error) => return Ok(Err(task_failure_from_adapter(error))),
    };
    if !phase2a_record_upload_id(conn, key, intent_token, &upload_id).map_err(pipeline_to_kio)? {
        return Ok(Ok(()));
    }
    if !phase2b_record_job_create_started(conn, key, intent_token).map_err(pipeline_to_kio)? {
        return Ok(Ok(()));
    }
    let job = match client.create_job(
        &upload_id,
        &batch_job_model(adapter_settings, ledger.control()),
        &metadata,
    ) {
        Ok(value) => value,
        Err(error) => return Ok(Err(task_failure_from_adapter(error))),
    };
    phase2b_record_job_created(conn, key, intent_token, &job.job_id).map_err(pipeline_to_kio)?;
    Ok(Ok(()))
}

/// §5.8 残骸掃除, cleanup-first: delete the upload (404 = success per the
/// trait contract), and only then NULL the `intent_token`
/// (`recovery_finish_cleanup`). A failed deletion leaves the token in place —
/// the filename-embedded token is the upload's only discovery key, and the
/// next pass (or reconcile/abandon) retries.
///
/// The stored provider scope is the exact authority for deletion. A missing
/// scope can only finish local cleanup when no upload was ever recorded:
/// when phase 2a started but `upload_id` was never recorded (the
/// upload-id-unrecorded crash window), an upload MAY exist that only the
/// `list_uploads` filename-token matching can find — clearing the token here
/// would make it permanently undiscoverable (04 §5.8 confirmed-absent note),
/// so this case keeps the token and defers to reconcile/abandon.
fn cleanup_batch_residue(
    ledger: &LedgerDb,
    client: &dyn MistralBatchClient,
    key: &LedgerTaskKey,
    intent_token: &str,
    upload_id: Option<&str>,
    stored_provider_scope: Option<&str>,
) -> Result<()> {
    match upload_id {
        Some(upload_id) => {
            if !batch_provider_scope_matches(stored_provider_scope, || client.provider_scope_id())
                || client.delete_upload(upload_id).is_err()
            {
                return Ok(());
            }
        }
        None if stored_provider_scope.is_some() => return Ok(()),
        // No phase 2a authority and no upload: cancellation before any send
        // needs no provider contact and must not strand a safe terminal row.
        None => {}
    }
    recovery_finish_cleanup(ledger, key, intent_token).map_err(pipeline_to_kio)?;
    Ok(())
}

/// 04 §5.8 相 3 terminal for a batch row, WITHOUT clearing `intent_token`
/// (cleanup-first: the token clears only via [`cleanup_batch_residue`] once
/// the upload deletion is confirmed). `ledger_batch_job_id` follows the §5.4
/// DDL 3-way rule: the REAL job id whenever it is known.
#[allow(clippy::too_many_arguments)]
fn settle_batch_charge_terminal(
    ledger: &LedgerDb,
    key: &LedgerTaskKey,
    intent_token: &str,
    ledger_batch_job_id: &str,
    outcome: Outcome,
    next_state: BatchState,
    error: Option<&str>,
    billed: BilledAmount,
    increment_contract_violation: bool,
    // §5.8: NULL 化は残骸掃除の完了時のみ. For the OCR lane that means never in
    // the collect Tx (an upload still has to be deleted). The embedding lane
    // submits INLINE, so it leaves no provider residue at all and terminal IS
    // cleanup-complete — the same reasoning §5.4 applies to sync rows. Leaving
    // the token set there would permanently block re-submission of that task
    // key ("旧 token の消し込み完了後にのみ再投入可").
    clear_intent_token: bool,
) -> Result<()> {
    terminal_transaction(
        ledger,
        &TerminalWrite {
            key,
            outcome,
            billed,
            ledger_batch_job_id,
            next_state,
            error,
            increment_contract_violation,
            attempts_delta: 1,
            clear_intent_token,
            intent_token_guard: Some(intent_token),
            reseat_submission_seq: false,
        },
    )
    .map_err(pipeline_to_kio)
    .map(|_| ())
}

/// What one polled batch row did to this pass's `ExecOutcome`.
enum BatchPollDisposition {
    Collected(ExecOutcome),
    InFlight,
    FailedPermanent,
    FailedRetryable,
    Retired,
}

/// 04 §5.8 相 3 (poll + collect) for this scope's in-flight batch markdownize
/// jobs — rows with a durably-known `batch_job_id` (`state IN (0,1)`; state=0
/// rows are reconcile-self-described crash windows, see
/// `ops::batch_poll_candidates`). Runs from `kio batch *` and the other
/// write-command entry points alongside `recover_stale_sync_rows`; polling is
/// receive/cleanup of an EXISTING request, never a new send, so it needs no
/// network opt-in (04 §5.8 回復 note). Rows whose job id is still unknown
/// stay owned by the §5.8 recovery walk (`kio ledger reconcile`) — no overlap
/// by construction.
fn poll_batch_markdownize_jobs(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    store: &TaskStore,
    ledger: &LedgerDb,
) -> Result<ExecOutcome> {
    let mut counts = ExecOutcome::default();
    let Some(client) = batch_markdownize_lane_client(adapter_settings, ledger)? else {
        return Ok(counts);
    };
    let scope_id = repo.scope_identity()?.scope_id;
    let rows = batch_poll_candidates(ledger, &scope_id, "markdownize").map_err(pipeline_to_kio)?;
    if !rows.is_empty() {
        let online_profile = online_markdownize_profile_for(repo)?;
        let placeholder_ref = online_output_ref(&online_profile.adapter_id);
        let tasks = store.all().map_err(pipeline_to_kio)?;
        for row in rows {
            let disposition = poll_one_batch_markdownize_row(
                adapter_settings,
                repo,
                store,
                ledger,
                client.as_ref(),
                &row,
                &online_profile,
                &placeholder_ref,
                &tasks,
            )?;
            match disposition {
                BatchPollDisposition::Collected(completion) => counts.add(completion),
                BatchPollDisposition::InFlight => counts.inflight += 1,
                BatchPollDisposition::FailedPermanent | BatchPollDisposition::Retired => {
                    counts.failed += 1;
                }
                BatchPollDisposition::FailedRetryable => {
                    counts.failed += 1;
                    counts.failed_retryable += 1;
                }
            }
        }
    }
    // §5.8 cleanup replay: this scope's terminal rows whose residue cleanup
    // did not complete (delete_upload failed, or a crash landed between the
    // terminal Tx and the cleanup) retry deletion here — "削除失敗・クラッシュ
    // は次回回復が再試行し、全削除の完了をもって intent_token を NULL 化する".
    for row in kio_pipeline::ledger::ops::recovery_candidates(ledger).map_err(pipeline_to_kio)? {
        if row.key.scope_id != scope_id
            || row.key.adapter_kind != "markdownize"
            || !row.state.is_terminal()
        {
            continue;
        }
        let Some(token) = row.intent_token.clone() else {
            continue;
        };
        cleanup_batch_residue(
            ledger,
            client.as_ref(),
            &row.key,
            &token,
            row.upload_id.as_deref(),
            row.provider_scope_id.as_deref(),
        )?;
    }
    Ok(counts)
}

/// No provider state or output is actionable until its durable job identity,
/// intent, and complete task key have all round-tripped unchanged.
fn poll_attributed_markdownize_batch_job(
    client: &dyn MistralBatchClient,
    row: &BatchRequestRow,
) -> Option<kio_adapter::batch_client::BatchJobRecord> {
    if !batch_provider_scope_matches(row.provider_scope_id.as_deref(), || {
        client.provider_scope_id()
    }) {
        return None;
    }
    let job_id = row.batch_job_id.as_deref()?;
    let intent_token = row.intent_token.as_deref()?;
    let job = client.get_job(job_id).ok()?;
    let task_key: kio_adapter::batch_inventory::ProviderTaskKey =
        serde_json::from_value(job.metadata.clone()).ok()?;
    if job.job_id != job_id
        || job.metadata.get("intent_token").and_then(Value::as_str) != Some(intent_token)
        || provider_task_ledger_key(&task_key) != row.key
    {
        return None;
    }
    Some(job)
}

#[allow(clippy::too_many_arguments)]
fn poll_one_batch_markdownize_row(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    store: &TaskStore,
    ledger: &LedgerDb,
    client: &dyn MistralBatchClient,
    row: &BatchRequestRow,
    online_profile: &AdapterProfile,
    placeholder_ref: &str,
    tasks: &[TaskDescriptor],
) -> Result<BatchPollDisposition> {
    let Some(job_id) = row.batch_job_id.clone() else {
        return Ok(BatchPollDisposition::InFlight);
    };
    let Some(intent_token) = row.intent_token.clone() else {
        // A state 0/1 row always carries a token by construction — defensive.
        return Ok(BatchPollDisposition::InFlight);
    };
    if row.key.tool_profile_hash != online_profile.tool_profile_hash {
        // A row reserved under a different (older) profile configuration:
        // this pass cannot attribute it to a live task; reconcile/abandon own
        // it (04 §5.8 unknown — leave untouched).
        return Ok(BatchPollDisposition::InFlight);
    }
    if !batch_provider_scope_matches(row.provider_scope_id.as_deref(), || {
        client.provider_scope_id()
    }) {
        return Ok(BatchPollDisposition::InFlight);
    }
    let task = tasks.iter().find(|task| {
        task.task_type == TaskType::Markdownize
            && task.input_hash == row.key.input_hash
            && matches!(
                task.status,
                TaskStatus::Pending | TaskStatus::Failed | TaskStatus::Running
            )
            && targets_standard_online_markdownize(store.kio_dir(), task, placeholder_ref)
    });
    let Some(task) = task else {
        // tasks.jsonl is loss-tolerated (04 §5.8): confirming + persisting an
        // output without a descriptor is the ledger-only recovery path
        // (gen/unit re-derivation), deliberately not wired in lane v1 —
        // `kio ledger reconcile` / `kio batch abandon` remain the owners.
        return Ok(BatchPollDisposition::InFlight);
    };
    let Some(job) = poll_attributed_markdownize_batch_job(client, row) else {
        return Ok(BatchPollDisposition::InFlight);
    };
    if job.status.is_in_flight() {
        extend_batch_poll_schedule(store, &task.task_id, repo.control())?;
        return Ok(BatchPollDisposition::InFlight);
    }
    if job.status != BatchJobStatus::Success {
        return settle_failed_batch_job(
            store,
            ledger,
            client,
            row,
            &intent_token,
            &job_id,
            task,
            &job.status,
        );
    }
    let Some(output_file_id) = job.output_file_id.clone() else {
        // SUCCESS without an output handle is a provider anomaly — treat as
        // still-unknown and re-poll rather than guessing.
        return Ok(BatchPollDisposition::InFlight);
    };
    if !batch_provider_scope_matches(row.provider_scope_id.as_deref(), || {
        client.provider_scope_id()
    }) {
        return Ok(BatchPollDisposition::InFlight);
    }
    let lines = match client.fetch_output(&output_file_id) {
        Ok(lines) => lines,
        Err(_) => return Ok(BatchPollDisposition::InFlight),
    };
    let line = lines
        .into_iter()
        .find(|line| line.custom_id == row.key.input_hash);
    collect_batch_markdownize_output(
        adapter_settings,
        repo,
        store,
        ledger,
        client,
        row,
        &intent_token,
        &job_id,
        task,
        online_profile,
        line,
    )
}

/// Terminal (non-success) provider job status → reject terminal (04 §5.8 相 3
/// reject 同形). Billed 0 confirmed (`usd=0`, `estimated=0`): Mistral does not
/// bill failed/cancelled batch entries (2026-07-03 verification premise;
/// orchestrator ruling for this lane — conservative in the "no phantom spend
/// held against the cap" direction; a resubmission is a human-driven fresh
/// phase 1). Outcome mapping inside the closed §5.4 enum: TIMEOUT_EXCEEDED is
/// the provider's job-expiry shape → `expired`; FAILED/CANCELLED/other →
/// `submit_rejected` (the provider-refused-the-work bucket — the enum has no
/// dedicated provider-job-failed value). `contract_violation_count` is NOT
/// incremented (a provider-side failure is not a §3.2 acceptance reject).
#[allow(clippy::too_many_arguments)]
fn settle_failed_batch_job(
    store: &TaskStore,
    ledger: &LedgerDb,
    client: &dyn MistralBatchClient,
    row: &BatchRequestRow,
    intent_token: &str,
    job_id: &str,
    task: &TaskDescriptor,
    status: &BatchJobStatus,
) -> Result<BatchPollDisposition> {
    let (outcome, error, billed) = match status {
        // Provider TIMEOUT_EXCEEDED: no output to read a real usage from —
        // the same unknown-cost epistemic state as a KIO-deadline expiry, so
        // it books the RESERVED estimate (04 §5.8 "expired = estimated 記帳",
        // over-count-safe) instead of assuming zero.
        BatchJobStatus::TimeoutExceeded => (
            Outcome::Expired,
            "expired",
            BilledAmount {
                usd: row.estimated_usd,
                estimated: true,
            },
        ),
        // FAILED / CANCELLED: the provider terminated the entry without
        // completing it; Mistral does not bill uncompleted batch entries, so
        // these settle at zero (comment pinned by contract test b4).
        BatchJobStatus::Cancelled => (
            Outcome::SubmitRejected,
            "cancelled",
            BilledAmount {
                usd: 0.0,
                estimated: false,
            },
        ),
        _ => (
            Outcome::SubmitRejected,
            "failed",
            BilledAmount {
                usd: 0.0,
                estimated: false,
            },
        ),
    };
    settle_batch_charge_terminal(
        ledger,
        &row.key,
        intent_token,
        job_id,
        outcome,
        BatchState::Terminal,
        Some(error),
        billed,
        false,
        false,
    )?;
    cleanup_batch_residue(
        ledger,
        client,
        &row.key,
        intent_token,
        row.upload_id.as_deref(),
        row.provider_scope_id.as_deref(),
    )?;
    mark_batch_task_failed_permanent(store, &task.task_id)?;
    Ok(BatchPollDisposition::FailedPermanent)
}

/// Provider job failure → the task joins the existing PERMANENT failure
/// mapping ("既存の失敗写像に合流"): `invalid_input` is the codebase's
/// non-retryable reason bucket (`retry_policy(InvalidInput).max_attempts=0`),
/// so `batch retry` does not auto-revive it — resubmission is a deliberate
/// human step (fresh phase 1 after re-index / explicit re-enqueue).
fn mark_batch_task_failed_permanent(store: &TaskStore, task_id: &str) -> Result<usize> {
    store
        .update_matching(|candidate| {
            if candidate.task_id == task_id {
                candidate.status = TaskStatus::Failed;
                candidate.fallback_reason =
                    Some(retry_reason(RetryErrorKind::InvalidInput).to_owned());
                candidate.hold_reason = None;
                candidate.heartbeat_at = None;
                candidate.attempts = candidate.attempts.saturating_add(1);
                candidate.next_retry_at = None;
                candidate.clear_reservation();
                true
            } else {
                false
            }
        })
        .map_err(pipeline_to_kio)
}

/// Extend a Pending batch task's poll schedule by one interval. A Failed task
/// (e.g. a crashed submit awaiting reconcile self-description) keeps its own
/// retry-policy schedule untouched.
fn extend_batch_poll_schedule(
    store: &TaskStore,
    task_id: &str,
    control: &ControlHandle,
) -> Result<()> {
    let poll_at = batch_poll_retry_at(control);
    store
        .update_matching(|candidate| {
            if candidate.task_id == task_id && candidate.status == TaskStatus::Pending {
                candidate.next_retry_at = Some(poll_at.clone());
                true
            } else {
                false
            }
        })
        .map_err(pipeline_to_kio)?;
    Ok(())
}

/// The Success + fetched-output half of one polled row: verify + materialize
/// through the SAME response half the sync lane uses, then run the §5.8 相 3
/// terminal Tx (成功 = usage-based charge like the sync settle; §3.2 reject =
/// contract_violation with the durable count), then cleanup-first residue
/// deletion, then the task transition.
#[allow(clippy::too_many_arguments)]
fn collect_batch_markdownize_output(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    store: &TaskStore,
    ledger: &LedgerDb,
    client: &dyn MistralBatchClient,
    row: &BatchRequestRow,
    intent_token: &str,
    job_id: &str,
    task: &TaskDescriptor,
    online_profile: &AdapterProfile,
    line: Option<BatchOutputLine>,
) -> Result<BatchPollDisposition> {
    let estimated = BilledAmount {
        usd: row.estimated_usd,
        estimated: true,
    };
    let Some(line) = line else {
        // A SUCCESS job whose output carries no line for our custom_id: a
        // transfer gap indistinguishable from provider data loss (04 §5.8's
        // 転送欠落 note). Conservative: settle at the reservation estimate
        // (over-count-safe), clean up, and leave the task permanently failed
        // for a human-driven fresh submission.
        settle_batch_charge_terminal(
            ledger,
            &row.key,
            intent_token,
            job_id,
            Outcome::UnknownSettled,
            BatchState::Terminal,
            Some("unknown_settled"),
            estimated,
            false,
            false,
        )?;
        cleanup_batch_residue(
            ledger,
            client,
            &row.key,
            intent_token,
            row.upload_id.as_deref(),
            row.provider_scope_id.as_deref(),
        )?;
        mark_batch_task_failed_permanent(store, &task.task_id)?;
        return Ok(BatchPollDisposition::FailedPermanent);
    };
    if line.status_code != 200 {
        // Per-entry provider failure inside a SUCCESS job — same reject
        // terminal as a failed job (Mistral does not bill failed entries).
        settle_batch_charge_terminal(
            ledger,
            &row.key,
            intent_token,
            job_id,
            Outcome::SubmitRejected,
            BatchState::Terminal,
            Some("failed"),
            BilledAmount {
                usd: 0.0,
                estimated: false,
            },
            false,
            false,
        )?;
        cleanup_batch_residue(
            ledger,
            client,
            &row.key,
            intent_token,
            row.upload_id.as_deref(),
            row.provider_scope_id.as_deref(),
        )?;
        mark_batch_task_failed_permanent(store, &task.task_id)?;
        return Ok(BatchPollDisposition::FailedPermanent);
    }
    // §5.8 相 3: persist 直前の tombstone 再検査 — purge 済みなら出力破棄、
    // reject 終端と同形 (error='purged')。課金は provider 報告値だが、この
    // lane の 報告値 (page count) は body の parse 成功が前提 — parse 前の
    // ここでは保守的に reservation estimate で確定する (over-count-safe)。
    if raw_ingest_is_purge_blocked(repo, &task.input_hash) {
        settle_batch_charge_terminal(
            ledger,
            &row.key,
            intent_token,
            job_id,
            Outcome::Purged,
            BatchState::Terminal,
            Some("purged"),
            estimated,
            false,
            false,
        )?;
        cleanup_batch_residue(
            ledger,
            client,
            &row.key,
            intent_token,
            row.upload_id.as_deref(),
            row.provider_scope_id.as_deref(),
        )?;
        retire_batch_task(store, &task.task_id)?;
        return Ok(BatchPollDisposition::Retired);
    }
    // Re-materialize the same input the submit used (the same precondition
    // gate the sync send runs): the output is persisted under the SUBMIT-time
    // identity (input_hash), so an edited/removed working file retires the
    // task instead of persisting under a stale identity.
    let prepared_input = match classify_online_markdownize_precondition(repo, task) {
        OnlineMarkdownizePrecondition::Send(prepared_input) => prepared_input,
        OnlineMarkdownizePrecondition::AwaitConversion
        | OnlineMarkdownizePrecondition::ConversionFailed => {
            // Transient local-material gap (e.g. office converter hiccup):
            // leave the row open and re-collect on a later pass.
            extend_batch_poll_schedule(store, &task.task_id, repo.control())?;
            return Ok(BatchPollDisposition::InFlight);
        }
        OnlineMarkdownizePrecondition::Retire => {
            // Superseded (edited/deleted since submit). The provider DID
            // complete the job; without a persistable target the charge is
            // settled at the conservative reservation estimate
            // (unknown_settled — the codebase's uniform non-success terminal)
            // and the residue is cleaned up.
            settle_batch_charge_terminal(
                ledger,
                &row.key,
                intent_token,
                job_id,
                Outcome::UnknownSettled,
                BatchState::Terminal,
                Some("unknown_settled"),
                estimated,
                false,
                false,
            )?;
            cleanup_batch_residue(
                ledger,
                client,
                &row.key,
                intent_token,
                row.upload_id.as_deref(),
                row.provider_scope_id.as_deref(),
            )?;
            retire_batch_task(store, &task.task_id)?;
            return Ok(BatchPollDisposition::Retired);
        }
    };
    // Persist identity: the resolved online profile (same resolution the sync
    // lane records instances under — seam-aware, network-free under the OCR
    // mock env). If resolution fails (e.g. no credentials in a passive
    // write-command entry poll), defer — never guess an identity.
    let resolved_profile = match resolve_standard_online_markdownize_profile_with_bbox_with_control(
        std::sync::Arc::clone(adapter_settings),
        &row.key.scope_id,
        task.bbox_annotation_enabled.unwrap_or(false),
        repo.control(),
    ) {
        Ok(profile) => profile,
        Err(_) => {
            extend_batch_poll_schedule(store, &task.task_id, repo.control())?;
            return Ok(BatchPollDisposition::InFlight);
        }
    };
    let retry_units: Option<BTreeSet<String>> = task
        .unit_keys
        .as_ref()
        .map(|keys| keys.iter().cloned().collect());
    // Keep the batch retry projection at the task boundary: only units that
    // were actually submitted may satisfy this collection. The adapter owns
    // parsing, image decoding/persistence, and provider-to-Kio conversion.
    let prepared_hints = if prepared_input.prepared_units.is_empty() && retry_units.is_none() {
        None
    } else {
        Some(
            prepared_input
                .prepared_units
                .iter()
                .filter(|unit| {
                    retry_units
                        .as_ref()
                        .is_none_or(|keys| keys.contains(&unit.unit_key))
                })
                .map(|unit| PreparedUnitHint {
                    unit_key: unit.unit_key.clone(),
                    prepared_hash: unit.prepared_hash.clone(),
                    unit_kind: adapter_unit_kind(unit.unit_type),
                    order: unit.order,
                })
                .collect::<Vec<_>>(),
        )
    };
    let converted = materialize_mistral_batch_ocr_body(
        &line.body,
        prepared_hints.as_deref(),
        &prepared_input.media_type,
        &task.input_hash,
        &row.key.scope_id,
        repo.kio_dir(),
        task.bbox_annotation_enabled.unwrap_or(false),
    );
    let (response, effective_hints) = match converted {
        Ok(converted) => converted,
        Err(BatchOcrMaterializationError::Contract(error)) => {
            // §3.2 acceptance reject (04 §5.8 相 3): terminal with the durable
            // contract_violation count. The provider usage report is part of
            // the very body that failed to parse, so the charge degrades to
            // the reservation estimate (§5.4 事前検証の縮退と同形). The
            // reason string lands in the event log — a silent reject cost a
            // live probe to diagnose on the 2026-07-23 OCR run (same lesson
            // as the offline `let _ = error` fix).
            repo.control().append_event_log_in_session(
                repo.lock_session(),
                "KIO-E-ADAPTER-CONTRACT-001",
                "batch OCR output failed acceptance; settled as contract_violation",
                json!({ "input_path": task.input_path, "detail": error.to_string() }),
            )?;
            return settle_batch_contract_reject(
                store,
                ledger,
                client,
                row,
                intent_token,
                job_id,
                task,
                estimated,
            );
        }
        Err(BatchOcrMaterializationError::Persistence(error)) => {
            // The provider result was already fetched and passed all response
            // validation. Keep the ledger reservation and collect it again;
            // treating local image CAS trouble as a contract reject would
            // discard a known billed result and schedule a new send.
            repo.control().append_event_log_in_session(
                repo.lock_session(),
                "KIO-E-BATCH-RESULT-STORE-001",
                "batch OCR image persistence deferred; retaining known provider result",
                json!({ "input_path": task.input_path, "detail": error.to_string() }),
            )?;
            extend_batch_poll_schedule(store, &task.task_id, repo.control())?;
            return Ok(BatchPollDisposition::InFlight);
        }
    };
    let usage = response.usage.clone();
    let materialized = materialize_online_markdownize_response(
        repo,
        task,
        prepared_input.prepared_units.clone(),
        &prepared_input.media_type,
        &prepared_input.bytes,
        retry_units.as_ref(),
        resolved_profile.tool_profile_hash.clone(),
        response,
        &effective_hints,
    );
    let outcome = match materialized {
        Ok(outcome) => outcome,
        Err(failure) if failure.retry_kind == RetryErrorKind::ContractViolation => {
            repo.control().append_event_log_in_session(repo.lock_session(),
                "KIO-E-ADAPTER-CONTRACT-001",
                "batch OCR materialize failed acceptance; settled as contract_violation",
                json!({ "input_path": task.input_path, "detail": format!("materialize reject: {:?}", failure.retry_kind) }),
            )?;
            return settle_batch_contract_reject(
                store,
                ledger,
                client,
                row,
                intent_token,
                job_id,
                task,
                estimated,
            );
        }
        Err(_failure) => {
            // Persist-side fault (I/O etc.): the ledger row stays open and the
            // fetched output is re-collected on a later pass (the §5.8 crash
            // window between collect and the terminal Tx, replayed).
            extend_batch_poll_schedule(store, &task.task_id, repo.control())?;
            return Ok(BatchPollDisposition::InFlight);
        }
    };
    // Success terminal: bill exactly as the sync settle does — the adapter
    // usage report resolved through declared kinds + tools.toml pricing,
    // degrading to the reservation estimate — but at the BATCH rate. This
    // collector only ever runs for a batch job, and it used to read the
    // declaration raw, i.e. at the sync rate.
    let markdownize_pricing =
        lane_adjusted_pricing(adapter_settings, "markdown", PreferredRequestKind::Batch);
    let (billed, uncovered_pricing_kinds) = resolve_billing_from_reported_usage(
        usage.as_ref(),
        &declared_billable_kind_strings(online_profile),
        &markdownize_pricing,
        row.estimated_usd,
    );
    warn_uncovered_pricing_kinds_once(
        "markdownize",
        &uncovered_pricing_kinds,
        repo.control(),
        repo.lock_session(),
    );
    settle_batch_charge_terminal(
        ledger,
        &row.key,
        intent_token,
        job_id,
        Outcome::Succeeded,
        BatchState::Completed,
        None,
        billed,
        false,
        false,
    )?;
    cleanup_batch_residue(
        ledger,
        client,
        &row.key,
        intent_token,
        row.upload_id.as_deref(),
        row.provider_scope_id.as_deref(),
    )?;
    let task_id = task.task_id.clone();
    store
        .update_matching(|candidate| {
            if candidate.task_id == task_id {
                candidate.status = outcome.status;
                candidate.output_ref = outcome.output_ref.clone();
                candidate.fallback_reason = Some("online_adapter_done".to_owned());
                candidate.hold_reason = None;
                candidate.heartbeat_at = None;
                candidate.next_retry_at = None;
                candidate.clear_reservation();
                if matches!(outcome.status, TaskStatus::Partial | TaskStatus::Failed) {
                    // Mirrors the sync Ok arm: every known incomplete response,
                    // including an all-Failed manifest, consumed one re-drive.
                    candidate.attempts = candidate.attempts.saturating_add(1);
                }
                candidate.mode = Some(outcome.mode);
                candidate.previous_raw_hash = outcome.previous_raw_hash.clone();
                candidate.parent_run_id = outcome.parent_run_id.clone();
                candidate.changed_unit_keys = outcome.changed_unit_keys.clone();
                true
            } else {
                false
            }
        })
        .map_err(pipeline_to_kio)?;
    Ok(BatchPollDisposition::Collected(
        known_markdownize_completion(store, &task_id)?,
    ))
}

/// §3.2 acceptance reject at collect time (04 §5.8 相 3): terminal Tx with
/// `error='contract_violation'` + the durable `contract_violation_count`
/// increment, charge at the conservative reservation estimate, cleanup-first
/// residue deletion, and the task joins the ordinary contract_violation retry
/// mapping (retryable once — a fresh phase 1 under a new token/seq).
#[allow(clippy::too_many_arguments)]
fn settle_batch_contract_reject(
    store: &TaskStore,
    ledger: &LedgerDb,
    client: &dyn MistralBatchClient,
    row: &BatchRequestRow,
    intent_token: &str,
    job_id: &str,
    task: &TaskDescriptor,
    estimated: BilledAmount,
) -> Result<BatchPollDisposition> {
    settle_batch_charge_terminal(
        ledger,
        &row.key,
        intent_token,
        job_id,
        Outcome::ContractViolation,
        BatchState::Terminal,
        Some("contract_violation"),
        estimated,
        true,
        false,
    )?;
    cleanup_batch_residue(
        ledger,
        client,
        &row.key,
        intent_token,
        row.upload_id.as_deref(),
        row.provider_scope_id.as_deref(),
    )?;
    let policy = retry_policy(RetryErrorKind::ContractViolation);
    let attempts_after = task.attempts.saturating_add(1);
    let next_retry_at = (policy.retryable
        && policy
            .max_attempts
            .map(|max| attempts_after < max)
            .unwrap_or(true))
    .then(|| {
        scheduled_retry_at(
            &ledger.control().now_utc_seconds(),
            &policy.backoff,
            attempts_after,
        )
    });
    let reason = retry_reason(RetryErrorKind::ContractViolation).to_owned();
    let task_id = task.task_id.clone();
    let retryable = next_retry_at.is_some();
    store
        .update_matching(|candidate| {
            if candidate.task_id == task_id {
                candidate.status = TaskStatus::Failed;
                candidate.fallback_reason = Some(reason.clone());
                candidate.hold_reason = None;
                candidate.heartbeat_at = None;
                candidate.attempts = attempts_after;
                candidate.next_retry_at = next_retry_at.clone();
                candidate.clear_reservation();
                true
            } else {
                false
            }
        })
        .map_err(pipeline_to_kio)?;
    Ok(if retryable {
        BatchPollDisposition::FailedRetryable
    } else {
        BatchPollDisposition::FailedPermanent
    })
}

/// Retire a batch task whose collect-time precondition failed (superseded /
/// purged) — same shape as the send-time `Retire` path.
fn retire_batch_task(store: &TaskStore, task_id: &str) -> Result<usize> {
    store
        .update_matching(|candidate| {
            if candidate.task_id == task_id {
                retire_online_task(candidate);
                candidate.attempts = candidate.attempts.saturating_add(1);
                true
            } else {
                false
            }
        })
        .map_err(pipeline_to_kio)
}

fn retire_online_task(task: &mut TaskDescriptor) {
    task.status = TaskStatus::Failed;
    task.fallback_reason = Some(RETIRED_NON_LIVE.to_owned());
    // A Failed task is not a hold — this shared helper can now retire a
    // Paused(auth) row too (QA2/QA3's supersede-on-edit / R18-2 orphan-sweep
    // callers admit Paused), so it must clear `hold_reason` itself.
    task.hold_reason = None;
    task.next_retry_at = None;
    task.heartbeat_at = None;
    task.clear_reservation();
}

/// Read-only enqueue classification is distinct from mutable billing authority.
#[derive(Clone, Copy)]
enum EnqueueBudget {
    Uninitialized,
    Available { device: f64, folder: Option<f64> },
}

#[allow(clippy::too_many_arguments)]
fn enqueue_online_placeholder_task(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    task_store: &TaskStore,
    candidate: &ScanCandidate,
    raw_hash: &str,
    scope_id: &str,
    network_allowed: bool,
    args: &IndexArgs,
    created_at: &str,
    result: &mut IndexPipelineResult,
    ledger: Option<&LedgerDb>,
    enqueue_budget: EnqueueBudget,
    budget_caps: &BudgetCaps,
    unsupported_store: &UnsupportedInputStore,
    unsupported_by_path: &mut BTreeMap<String, UnsupportedInputDisposition>,
    office_conversion_unavailable_paths: &mut BTreeSet<String>,
) -> Result<()> {
    // R9-2: text-native files (Markdown / plain text / code) are fully handled by
    // the deterministic Adapter (07 §2.1) and must never enqueue an online OCR task
    // (07 §5.2 scopes Mistral OCR to non-text-native PDF/DOCX/PPTX/images). Gate at
    // enqueue so a routine `index` never creates a redundant, privacy-leaking,
    // billed task for a `.md` / `.txt` / code file.
    if is_text_native_media(&candidate.media_type) {
        return Ok(());
    }
    // 07 §5.1: an Office (DOCX/PPTX) file whose converter is not resolvable
    // right now must NEVER get a doomed online task -- there is no converted
    // PDF to send. Record the skip (idempotent: `record_unsupported_if_changed`
    // no-ops when the same disposition is already the latest row for this
    // path) so `compute_index_status`'s `office_conversion_unavailable`
    // discloses WHY this file has no enrichment task, instead of a silent gap.
    // Once a converter resolves (a later `kio index`), fall through to the
    // normal enqueue path below and record the transition back to resolved.
    if is_office_media(&candidate.media_type) {
        if resolve_office_converter_with_control(repo.control()).is_none() {
            record_unsupported_if_changed(
                unsupported_store,
                unsupported_by_path,
                UnsupportedInputDisposition {
                    path: candidate.input_path.clone(),
                    raw_hash: raw_hash.to_owned(),
                    media_type: candidate.media_type.clone(),
                    size_bytes: candidate.size_bytes,
                    reason: OFFICE_CONVERSION_UNAVAILABLE_REASON.to_owned(),
                },
            )?;
            office_conversion_unavailable_paths.insert(candidate.input_path.clone());
            return Ok(());
        }
        if office_conversion_unavailable_paths.remove(&candidate.input_path) {
            record_unsupported_if_changed(
                unsupported_store,
                unsupported_by_path,
                UnsupportedInputDisposition {
                    path: candidate.input_path.clone(),
                    raw_hash: raw_hash.to_owned(),
                    media_type: candidate.media_type.clone(),
                    size_bytes: candidate.size_bytes,
                    reason: UNSUPPORTED_REASON_RESOLVED.to_owned(),
                },
            )?;
        }
    }
    let online_profile = online_markdownize_profile_for(repo)?;
    // Stage 3: a declared `offline_api` pipeline owns this enrichment instead
    // of the cloud OCR. Resolved once, here, so the task's `output_ref` and the
    // status decision below cannot disagree about which route it is on.
    let local_ocr = active_local_ocr_execution_with_control(adapter_settings, repo.control())
        .filter(|_| local_ocr_handles_media_type(&candidate.media_type));
    let (output_ref, secrets_approved) = match local_ocr {
        Some(execution) => {
            let profile = kio_adapter::local_ocr_markdownize::profile_for(execution);
            let approved = secrets_send_approved(
                adapter_settings,
                repo,
                approvals::AdapterRole::Markdown,
                &profile.adapter_id,
                &profile.tool_profile_hash,
            );
            (offline_output_ref(&profile.adapter_id), approved)
        }
        None => (
            online_output_ref(&online_profile.adapter_id),
            secrets_send_approved(
                adapter_settings,
                repo,
                approvals::AdapterRole::Markdown,
                &online_profile.adapter_id,
                &online_profile.tool_profile_hash,
            ),
        ),
    };
    let secrets_hold = !secrets_approved && classify_secret(&candidate.input_path).is_some();
    let effective_bbox_policy = bbox_annotation_enabled(repo)?;
    // R15-2: supersede any stale online markdownize task for THIS path whose
    // `input_hash` differs from the current content. The file was edited after that
    // task was enqueued, so it is stale: `batch resume` would R14-2-supersede a
    // Pending/Paused one at send time (adapter never called) but only AFTER the
    // pre-send reservation already landed, and left unretired these stale tasks
    // accumulate and eat the per-adapter markdownize cap, falsely pausing the
    // current (valid) task. Retire them here (non-retryable `invalid_input`; the
    // state machine has no "superseded" state and the recovery is this fresh
    // re-index, not a retry). Runs before the idempotency check so it fires even
    // when a same-hash task also exists.
    //
    // R17-3 (still applicable under the SQLite ledger): extend the retirement to a
    // RETRYABLE `Failed` task (gated by `task_retry_allowed`, so a
    // permanently-failed task is left alone) — a Failed(rate_limit/…) task keeps
    // its ledger row open across retries (`reserve_or_reuse_task_charge`'s reuse
    // path), so it must be explicitly released here or the stale row lingers,
    // eating the per-adapter markdownize cap. Released via
    // `release_task_charge_if_open` (settles `unknown_settled` at the reservation
    // estimate if a row is still open; a no-op otherwise) — under the same
    // `reservation_scope_id` the send path reserves with
    // (`repo.scope_identity()?.scope_id`), so the release nets out at the
    // folder-scoped totals too, not just the device total.
    let markdownize_adapter_kind = "markdownize";
    let reservation_scope_id = repo.scope_identity()?.scope_id;
    let mut stale_ids = BTreeSet::new();
    for task in task_store.all().map_err(pipeline_to_kio)? {
        let stale = task.task_type == TaskType::Markdownize
            && targets_standard_online_markdownize(task_store.kio_dir(), &task, &output_ref)
            && task.input_path == candidate.input_path
            && task.input_hash != raw_hash
            && (matches!(task.status, TaskStatus::Pending | TaskStatus::Paused)
                || (task.status == TaskStatus::Failed
                    && is_reservation_bearing_send_failure(&task)));
        if !stale {
            continue;
        }
        let key = task_ledger_key(
            &reservation_scope_id,
            markdownize_adapter_kind,
            &task.input_hash,
            &online_profile.tool_profile_hash,
        );
        if let Some(ledger) = ledger {
            release_task_charge_if_open(ledger, &key)?;
        }
        stale_ids.insert(task.task_id);
    }
    task_store
        .update_matching(|task| {
            if !stale_ids.contains(&task.task_id) {
                return false;
            }
            retire_online_task(task);
            true
        })
        .map_err(pipeline_to_kio)?;
    // Idempotency (Step2c I1, 04 §5.5): never enqueue a second online task for an
    // identity `(input_path, input_hash)` that already has an online task in any
    // live lifecycle state. Without the completed-state check, every unchanged
    // re-index appended a fresh task, and a later `batch resume` re-sent it to
    // the API and double-charged the ledger. A changed file produces a new
    // `input_hash`, so it still enqueues.
    //
    // The online task is identified differently across its lifecycle:
    //   * Pending/Paused (not yet executed) still carries the placeholder
    //     `output_ref`.
    //   * Done/Partial (executed) has had its `output_ref` rewritten to the
    //     normalized-instance path, but `execute_online_markdownize_task`
    //     stamps `fallback_reason = "online_adapter_done"`.
    //   * Failed/Running are also deduped: re-enqueueing a fresh Pending task
    //     (next_retry_at = None) would bypass the retry backoff gate and cause
    //     an extra API call. Failed tasks are owned by `kio batch retry`, which
    //     honors `next_retry_at` and the per-error retry budget.
    if let Some(existing) = task_store
        .all()
        .map_err(pipeline_to_kio)?
        .into_iter()
        .find(|task| {
            if task.input_path != candidate.input_path || task.input_hash != raw_hash {
                return false;
            }
            if task.bbox_annotation_enabled != Some(effective_bbox_policy) {
                return false;
            }
            match task.status {
                TaskStatus::Pending | TaskStatus::Paused => {
                    targets_standard_online_markdownize(task_store.kio_dir(), task, &output_ref)
                }
                TaskStatus::Done | TaskStatus::Partial => {
                    task.fallback_reason.as_deref() == Some("online_adapter_done")
                }
                // R19-3: a `retired_non_live` Failed task must NOT block a fresh enqueue
                // when the (path,hash) identity reappears (revert/restore). Other Failed
                // tasks stay deduped (owned by `batch retry`, which honors backoff).
                TaskStatus::Failed => {
                    targets_standard_online_markdownize(task_store.kio_dir(), task, &output_ref)
                        && task.fallback_reason.as_deref() != Some(RETIRED_NON_LIVE)
                }
                TaskStatus::Running => {
                    targets_standard_online_markdownize(task_store.kio_dir(), task, &output_ref)
                }
            }
        })
    {
        match existing.status {
            TaskStatus::Paused => result.paused_tasks += 1,
            TaskStatus::Pending => result.pending_online_tasks += 1,
            _ => {}
        }
        return Ok(());
    }

    let estimate = BudgetEstimate {
        scope_id: scope_id.to_owned(),
        task_type: TaskType::Markdownize,
        estimated_usd: estimate_online_markdownize_cost(
            candidate.size_bytes,
            effective_bbox_policy,
        ),
        adapter_id: Some(online_profile.adapter_id.clone()),
    };
    let budget = match enqueue_budget {
        EnqueueBudget::Available { device, folder } => {
            evaluate_budget_with_caps(&estimate, device, folder, matches_batch_override(args))
        }
        EnqueueBudget::Uninitialized => {
            evaluate_budget_with_caps(&estimate, f64::NEG_INFINITY, Some(f64::NEG_INFINITY), false)
        }
    };
    // R10-7: an online markdownize task can only ever be DRIVEN by `batch`, whose
    // gate is the persistent per-adapter opt-in (`persistent_network_allowed`). A
    // one-shot `--online --yes` sets per-invocation `network_allowed = true` but
    // leaves no persistent opt-in, so the task can never be sent — a silent dead-end.
    // Report the honest state (`network_opt_in_required`) rather than a false
    // `ready_for_online_adapter`. (Inline `--yes` sending, to match embedding, is
    // deferred with the F6 promotion wiring.)
    let markdownize_drivable =
        network_allowed && persistent_network_allowed(adapter_settings, repo)?;
    // N1a: a Tier B (candidate-secret) file without `--send-secrets` is held here
    // — a Paused task with `secrets_tier_b_hold`, visible in `kio status`, that
    // `batch resume` will not un-hold and `execute_pending_markdownize_tasks` will
    // not send. The hold takes precedence over budget/network reasons.
    let (status, reason) = if local_ocr.is_some() {
        offline_markdownize_enqueue_status(secrets_hold)
    } else if secrets_hold {
        (TaskStatus::Paused, Some(SECRETS_TIER_B_HOLD))
    } else if matches!(enqueue_budget, EnqueueBudget::Uninitialized) {
        (
            TaskStatus::Paused,
            Some(LEDGER_INITIALIZATION_REQUIRED_REASON),
        )
    } else if !budget.allowed && budget_caps.hard_stop {
        // F5: only a hard-stop cap pauses the task. Under soft-stop the online task
        // is enqueued normally and its over-cap charge is recorded at execution.
        (TaskStatus::Paused, Some("budget_exceeded"))
    } else if !markdownize_drivable {
        (TaskStatus::Pending, Some("network_opt_in_required"))
    } else {
        (TaskStatus::Pending, Some("ready_for_online_adapter"))
    };
    let mut task = task_descriptor(
        repo,
        TaskType::Markdownize,
        Some(MarkdownizeMode::Full),
        candidate,
        raw_hash,
        &output_ref,
        status,
        reason,
        created_at,
    );
    task.bbox_annotation_enabled = Some(effective_bbox_policy);
    task_store.append(&task).map_err(pipeline_to_kio)?;
    match status {
        TaskStatus::Paused => result.paused_tasks += 1,
        TaskStatus::Pending => result.pending_online_tasks += 1,
        _ => {}
    }
    Ok(())
}

fn matches_batch_override(_args: &IndexArgs) -> bool {
    false
}

/// This month's remaining `(device, folder)` budget for the `(scope, adapter)`
/// filter, shared by the enqueue-time pre-check
/// (`enqueue_online_placeholder_task`/`enqueue_embedding_tasks`'s siblings), the
/// send-time check-then-reserve (`reserve_or_reuse_task_charge`, which
/// independently re-checks atomically inside its own `BEGIN IMMEDIATE` Tx — this
/// function is for the READ-ONLY enqueue-time estimate, not a substitute for
/// that atomicity), and the status/warning reports (`budget_status_json` /
/// `scope_budget_warning`). `ledger_month_total` (CL59) already sums confirmed
/// `cost_ledger` charges plus unterminated `batch_requests` reservations — the
/// retired JSONL design's separate reclaim-ledger netting
/// (`net_monthly_spent`/R17-3/R18-3) has no equivalent here: this ledger never
/// records a charge until a definitive terminal Tx, so there is nothing to
/// reclaim after the fact (see the implementation report).
fn budget_remaining_for_adapter(
    ledger: &LedgerDb,
    budget_caps: &BudgetCaps,
    scope_id: &str,
    adapter_kind: &str,
) -> Result<(f64, Option<f64>)> {
    let month = utc_month(&ledger.control().now_utc_seconds());
    let conn = &ledger;
    let device_spent = ledger_month_total(conn, None, None, &month).map_err(pipeline_to_kio)?;
    let folder_spent =
        ledger_month_total(conn, Some(scope_id), None, &month).map_err(pipeline_to_kio)?;
    let device_adapter_spent =
        ledger_month_total(conn, None, Some(adapter_kind), &month).map_err(pipeline_to_kio)?;
    Ok(budget_remaining_from_totals(
        budget_caps,
        adapter_kind,
        device_spent,
        folder_spent,
        device_adapter_spent,
    ))
}

fn budget_remaining_from_totals(
    budget_caps: &BudgetCaps,
    adapter_kind: &str,
    device_spent: f64,
    folder_spent: f64,
    device_adapter_spent: f64,
) -> (f64, Option<f64>) {
    let mut device_remaining = budget_caps.device_monthly_usd_cap - device_spent;
    if let Some(adapter_cap) = budget_caps.device_per_adapter.get(adapter_kind) {
        device_remaining = device_remaining.min(adapter_cap - device_adapter_spent);
    }
    // QA11 (step4b-contract-tests-p3a.md §D): folder per_adapter does not
    // exist (04 §5.4 — "folder cap は total のみ"); this estimate must not
    // narrow `folder_remaining` for a constraint the Tx-atomic gate
    // (`check_then_reserve`/`BudgetCapConfig`) never applies either — a real
    // 2-condition gate (device cap, folder total cap) staying consistent
    // with a 3-condition estimate silently added a THIRD gate here that
    // could pause a task the atomic check would have allowed.
    let folder_remaining = budget_caps
        .folder_monthly_usd_cap
        .map(|cap| cap - folder_spent);
    (device_remaining, folder_remaining)
}

fn materialize_tool_lock(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
) -> Result<()> {
    let prepare_profile = builtin_prepare_profile();
    let markdown_profile = active_markdown_adapter(repo).profile();
    let mut value = json!({
        "spec_version": 1,
        "prepare": {
            "tool_id": prepare_profile.adapter_id,
            "profile_hash": prepare_profile.tool_profile_hash,
            "kind": "deterministic_library"
        },
        "markdown": {
            "tool_id": markdown_profile.adapter_id,
            "profile_hash": markdown_profile.tool_profile_hash,
            "kind": execution_mode_name(markdown_profile.execution_mode),
            "capabilities": markdown_profile.capability_flags
        }
    });
    // Write the embedding entry (07 §6) when an embedding adapter is configured.
    // A non-multimodal profile is rejected here (03 §7): `load_tool_lock` fails
    // with KIO-E-EMBED-MODALITY-001, which we surface as exit 2 (scenario (e))
    // *before* any indexing happens.
    if let Some(entry) = embedding_tool_lock_entry(adapter_settings, repo.control())?
        && let Some(object) = value.as_object_mut()
    {
        object.insert("embedding".to_owned(), entry);
    }
    let bytes =
        serde_json::to_vec_pretty(&value).map_err(|err| KioError::schema(err.to_string()))?;
    // `adapter_to_kio` reads the code off `ConfigSchemaCoded` and gives it
    // exit 2, so KIO-E-EMBED-MODALITY-001 needs no special case here.
    load_tool_lock(&bytes).map_err(adapter_to_kio)?;
    let path = repo.kio_dir().join("tool-lock.json");
    atomic_overwrite_file(&path, &bytes)
}

fn execution_mode_name(mode: ExecutionMode) -> &'static str {
    match mode {
        ExecutionMode::OnlineApi => "online_api",
        ExecutionMode::OfflineApi => "offline_api",
        ExecutionMode::DeterministicLibrary => "deterministic_library",
    }
}

fn network_allowed(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    args: &IndexArgs,
) -> Result<bool> {
    if args.offline {
        return Ok(false);
    }
    persistent_network_allowed(adapter_settings, repo)
}

/// Current paired device and scope permission for this exact secret recipient.
fn secrets_send_approved(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    role: approvals::AdapterRole,
    tool_id: &str,
    profile: &str,
) -> bool {
    approvals::secrets_allowed(adapter_settings, repo, role, tool_id, profile).unwrap_or(false)
}

fn active_markdownize_profile_for(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
) -> Result<AdapterProfile> {
    match active_local_ocr_execution_with_control(adapter_settings, repo.control()) {
        Some(execution) => Ok(kio_adapter::local_ocr_markdownize::profile_for(execution)),
        None => online_markdownize_profile_for(repo),
    }
}

fn markdown_output_ref_for_profile(profile: &AdapterProfile) -> String {
    match profile.execution_mode {
        ExecutionMode::OfflineApi => offline_output_ref(&profile.adapter_id),
        ExecutionMode::OnlineApi => online_output_ref(&profile.adapter_id),
        ExecutionMode::DeterministicLibrary => unreachable!("OCR profiles are external adapters"),
    }
}

/// Release only the Markdownize holds whose exact selected external recipient
/// has a current send-secrets grant. Embedding holds are re-driven by their
/// separate embedding admission path.
fn release_markdownize_secret_holds(repo: &Repository, output_ref: &str) -> Result<usize> {
    let store = TaskStore::new_with_control(repo.kio_dir(), repo.control().clone());
    let released = store
        .update_matching(|task| {
            if task.task_type == TaskType::Markdownize
                && task.output_ref == output_ref
                && task.status == TaskStatus::Paused
                && task.fallback_reason.as_deref() == Some(SECRETS_TIER_B_HOLD)
            {
                task.status = TaskStatus::Pending;
                task.fallback_reason = None;
                task.hold_reason = None;
                true
            } else {
                false
            }
        })
        .map_err(pipeline_to_kio)?;
    Ok(released)
}

/// A terminal embedding failure must remain terminal when a later secret hold
/// with the same output reference is encountered. Remove only that redundant
/// hold before the ordinary embedding hold/enqueue reconciliation runs.
fn prune_terminal_embedding_secret_holds(repo: &Repository) -> Result<usize> {
    let store = TaskStore::new_with_control(repo.kio_dir(), repo.control().clone());
    let mut tasks = store.all().map_err(pipeline_to_kio)?;
    let terminal_embedding_refs = tasks
        .iter()
        .filter(|task| {
            task.task_type == TaskType::Embedding
                && task.status == TaskStatus::Failed
                && task.fallback_reason.as_deref() != Some(RETIRED_NON_LIVE)
                && !task_retry_allowed(task)
        })
        .map(|task| task.output_ref.clone())
        .collect::<BTreeSet<_>>();
    let before = tasks.len();
    tasks.retain(|task| {
        !(task.task_type == TaskType::Embedding
            && task.status == TaskStatus::Paused
            && task.fallback_reason.as_deref() == Some(SECRETS_TIER_B_HOLD)
            && terminal_embedding_refs.contains(&task.output_ref))
    });
    let removed_duplicates = before.saturating_sub(tasks.len());
    if removed_duplicates > 0 {
        store.replace_all(&tasks).map_err(pipeline_to_kio)?;
    }
    Ok(removed_duplicates)
}

/// Fallback reason marking an online task held because its input is a Tier B
/// (candidate-secret) file awaiting `--send-secrets` (N1a).
const SECRETS_TIER_B_HOLD: &str = "secrets_tier_b_hold";

fn record_quarantine_candidates(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    preview: &ScanPreview,
) -> Result<()> {
    let path = repo.kio_dir().join("quarantine.jsonl");
    // R19-7: dedup on (path, approval_method), not path alone — otherwise a
    // `hold` record permanently blocks the later `send_approved` transition
    // (after `--send-secrets`), leaving `kio status` reporting an approved,
    // already-sent file as still pending-hold. The reader takes the latest row
    // per path, so appending the transition row corrects the disposition.
    let existing = read_quarantine_records(repo)?
        .into_iter()
        .filter_map(|entry| {
            let path = entry.get("path").and_then(Value::as_str)?.to_owned();
            let method = entry
                .get("approval_method")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            Some((path, method))
        })
        .collect::<BTreeSet<_>>();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|err| KioError::io(err.to_string(), path.display().to_string()))?;
    for candidate in &preview.candidates {
        let profile =
            match active_local_ocr_execution_with_control(adapter_settings, repo.control())
                .filter(|_| local_ocr_handles_media_type(&candidate.media_type))
            {
                Some(execution) => kio_adapter::local_ocr_markdownize::profile_for(execution),
                None => online_markdownize_profile_for(repo)?,
            };
        let secrets_approved = secrets_send_approved(
            adapter_settings,
            repo,
            approvals::AdapterRole::Markdown,
            &profile.adapter_id,
            &profile.tool_profile_hash,
        );
        // N1b: record Tier A (excluded from ingest) AND Tier B (ingested locally
        // but held from online send) so `kio status` surfaces both. Tier B carries
        // its live disposition — "hold" until `--send-secrets`, then "send_approved".
        let (reason, approval_method) = match candidate.quarantine_reason.as_deref() {
            Some("secrets_tier_a_excluded") if candidate.ignored => {
                ("secrets_tier_a", "quarantine")
            }
            // R19-1: a lifted (ingested) Tier A secret held from online send — record it
            // with its live disposition so `kio status` surfaces the pending online hold
            // and the eventual approval (audit trail; 07 §122).
            Some("secrets_tier_a_online_hold") => (
                "secrets_tier_a",
                if secrets_approved {
                    "send_approved"
                } else {
                    "hold"
                },
            ),
            Some("secrets_tier_b_warning") => (
                "secrets_tier_b",
                if secrets_approved {
                    "send_approved"
                } else {
                    "hold"
                },
            ),
            _ => continue,
        };
        if existing.contains(&(candidate.input_path.clone(), approval_method.to_owned())) {
            continue;
        }
        let record = json!({
            "path": candidate.input_path,
            "reason": reason,
            "recorded_at": repo.control().now_utc_seconds(),
            "approval_method": approval_method,
        });
        // M1(b): one framed record per single write_all (no interleaving).
        let mut line = serde_json::to_string(&record)
            .map_err(|err| KioError::io(err.to_string(), path.display().to_string()))?;
        line.push('\n');
        file.write_all(line.as_bytes())
            .map_err(|err| KioError::io(err.to_string(), path.display().to_string()))?;
    }
    Ok(())
}

fn read_quarantine_records(repo: &Repository) -> Result<Vec<Value>> {
    let path = repo.kio_dir().join("quarantine.jsonl");
    let Ok(text) = fs::read_to_string(&path) else {
        return Ok(Vec::new());
    };
    Ok(text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect())
}

/// R20-9: quarantine records collapsed to the LATEST disposition per path, for `kio status`
/// display. `record_quarantine_candidates` appends a `hold` row, then (after
/// `--send-secrets`) a `send_approved` row — R19-7's `(path, approval_method)` dedup lets
/// both land. The raw `read_quarantine_records` returns BOTH, so status reported a file as
/// simultaneously "hold" and "send_approved". quarantine.jsonl is append-only in
/// chronological order, so the last row per path is the current disposition — the reader
/// behavior R19-7's fix comment assumed but never implemented. The raw reader is left
/// untouched (the writer's own `(path, method)` dedup set needs every row).
fn quarantine_status_records(repo: &Repository) -> Result<Vec<Value>> {
    let mut order: Vec<String> = Vec::new();
    let mut latest: BTreeMap<String, Value> = BTreeMap::new();
    for row in read_quarantine_records(repo)? {
        let key = row
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if !latest.contains_key(&key) {
            order.push(key.clone());
        }
        latest.insert(key, row);
    }
    Ok(order
        .into_iter()
        .filter_map(|key| latest.remove(&key))
        .collect())
}

/// QA7 (step4b-contract-tests-p3a.md §B, arbitration #1, 10 §1.1 L128-130):
/// `effective_ignore_hash` hashes the ACTUAL Tier A/B pattern content (via
/// `kio_core::scope::tier_a_template_text`), not a hand-maintained version
/// literal — a pattern-list edit changes this hash automatically.
fn effective_ignore_hash() -> String {
    hash_bytes(kio_core::scope::tier_a_template_text(kio_pipeline::scan::TIER_B_NEEDLES).as_bytes())
}

fn write_approval_record(
    repo: &Repository,
    preview: &ScanPreview,
    approval_method: &str,
) -> Result<()> {
    // QA5 (step4b-contract-tests-p3a.md §B, 10 §1 L97-113): the one-time
    // scope-level scan approval, distinct from the adapter-level opt-in rows
    // below (`approvals.jsonl`/`consents.jsonl`) — `record_scan_approval` is
    // idempotent, so calling it again on a later `index --approve` with no
    // NEW adapter to opt in (the `pending.is_empty()` early return below) is
    // harmless. QA19 (step4b-contract-tests-p3a.md §F, 10 §1 L48-53): cost
    // estimates come from `preview.estimated_cost` (`kio-pipeline`'s
    // `build_scan_preview`, now wired to `tools.toml`'s declared `[pricing]`)
    // — `0.0` only when no `pricing` is declared for that role, an honest
    // "unknown" rather than a fabricated figure.
    let (estimated_markdownize_usd, estimated_embedding_usd) = preview
        .estimated_cost
        .as_ref()
        .map(|cost| (cost.estimated_markdownize_usd, cost.estimated_embedding_usd))
        .unwrap_or((0.0, 0.0));
    repo.record_scan_approval(json!({
        "scope_id": preview.scope_id,
        "root_path": repo.canonical_root().display().to_string(),
        "approved_at": repo.control().now_utc_seconds(),
        "actor": std::env::var("USER").unwrap_or_else(|_| "unknown".to_owned()),
        "approval_method": approval_method,
        "kio_version": env!("CARGO_PKG_VERSION"),
        "effective_ignore_hash": effective_ignore_hash(),
        "estimated_file_count": preview.candidates.iter().filter(|candidate| !candidate.ignored).count(),
        "estimated_total_bytes": preview.candidates.iter().filter(|candidate| !candidate.ignored).map(|candidate| candidate.size_bytes).sum::<u64>(),
        "estimated_markdownize_usd": estimated_markdownize_usd,
        "estimated_embedding_usd": estimated_embedding_usd,
    }))?;

    Ok(())
}

/// PB04 (step4b-contract-tests-p2b.md §B; 10 §7.5.1 L493-497; 03 §8.1):
/// compute this normalized instance's *current* manifest.json canonical JCS
/// content hash and durably CAS-write it as a `ContentObjectKind::Manifest`
/// object (`objects/manifests/` — 03 §2.1), for `NormalizeRef::manifest_hash`
/// (tree schema v2). Reuses the same provenance-rebinding loader
/// `verify_objects`/purge's orphan scan already use rather than reading
/// `manifest.json` bytes ad hoc, so the hashed content is bound to the
/// requested `(raw_hash, tool_profile_hash, gen)` identity. `write_content_object`
/// is idempotent (verifies existing bytes rather than re-writing), matching
/// same-gen finalize's repeated manifest updates (03 §8, "unit の failed →
/// done 遷移で変わるため" — each transition yields a new manifest_hash).
///
/// This is a strict provenance boundary: callers must propagate any failure.
/// Every current-format `NormalizeRef` carries the returned manifest hash;
/// there is no missing-hash compatibility shape.
fn compute_manifest_hash(
    repo: &Repository,
    raw_hash: &str,
    tool_profile_hash: &str,
    r#gen: u64,
) -> Result<String> {
    let instance = load_validated_normalized_instance_with_control(
        repo.kio_dir(),
        raw_hash,
        tool_profile_hash,
        r#gen,
        repo.control(),
    )
    .map_err(pipeline_to_kio)?;
    hash_and_write_manifest_object(repo, &instance.manifest)
}

/// Low-level half of [`compute_manifest_hash`]: canonicalize an
/// already-in-memory `NormalizedInstanceManifest` and CAS-write it. Callers
/// that just finished `persist_normalized_instance` (the manifest is already
/// in hand, no need to re-read+re-validate it from disk) use this directly.
fn hash_and_write_manifest_object(
    repo: &Repository,
    manifest: &NormalizedInstanceManifest,
) -> Result<String> {
    let value =
        serde_json::to_value(manifest).map_err(|error| KioError::schema(error.to_string()))?;
    let bytes = canonical_json_bytes(&value)?;
    repo.object_store()
        .write_content_object(ContentObjectKind::Manifest, &bytes)
}

fn adapter_to_kio(error: kio_adapter::AdapterError) -> KioError {
    match error {
        kio_adapter::AdapterError::LocalPeerAuth(message) => KioError::new(
            "KIO-E-LOCAL-PEER-AUTH-001",
            message,
            json!({}),
            ExitCode::AuthError,
        ),
        kio_adapter::AdapterError::LocalPeerConfig { code, message } => {
            KioError::new(code, message, json!({}), ExitCode::InvalidUsage)
        }
        // A config violation that named its own operator-facing code (06 §8).
        // `AdapterError::error_code()` cannot carry it — that one is pinned to
        // the retry-policy table — so the code travels on the variant and is
        // read structurally here rather than sniffed out of the message text.
        // Every code in this class is exit 2.
        kio_adapter::AdapterError::ConfigSchemaCoded { code, message } => {
            KioError::new(code, message, json!({}), ExitCode::InvalidUsage)
        }
        other => KioError::schema(other.to_string()),
    }
}

fn pipeline_to_kio(error: kio_pipeline::PipelineError) -> KioError {
    match error {
        // M1(c): a corrupt persisted store file is exit 4 (KIO-E-STORE-CORRUPT-001),
        // not a schema/config error (exit 2). The path is preserved in context.
        kio_pipeline::PipelineError::Corrupt { path, message } => KioError::new(
            "KIO-E-STORE-CORRUPT-001",
            format!("corrupt store file at {path}: {message}"),
            json!({ "path": path }),
            ExitCode::PermanentFailure,
        ),
        // P1: a task whose input_path escapes the scope is KIO-E-STORE-PATH-001
        // (exit 2), the same contract as an out-of-scope tree entry path. The
        // offending path stays in context (redacted in logs), never in the message.
        kio_pipeline::PipelineError::Path { path } => KioError::new(
            "KIO-E-STORE-PATH-001",
            "task input_path must be a scope-local file name (no separators or `..` traversal)",
            json!({ "path": path }),
            ExitCode::InvalidUsage,
        ),
        kio_pipeline::PipelineError::Locked { path } => KioError::locked(path),
        kio_pipeline::PipelineError::Io { path, message } => KioError::io(message, path),
        kio_pipeline::PipelineError::LedgerSnapshot { path, source } => {
            ledger_snapshot_to_kio(&path, source)
        }
        // Lifecycle errors distinguish an explicit setup operation, recoverable
        // incomplete state, and unsafe durable state. A caller must not retry
        // a missing authority as if it were a transient provider failure.
        kio_pipeline::PipelineError::Contract { code, message } => {
            let exit = if matches!(
                code,
                "KIO-E-LEDGER-UNINITIALIZED-001"
                    | "KIO-E-LEDGER-ALREADY-EXISTS-001"
                    | "KIO-E-LEDGER-PATH-001"
            ) {
                ExitCode::InvalidUsage
            } else if matches!(
                code,
                "KIO-E-LEDGER-INIT-PARTIAL-001" | "KIO-E-LEDGER-SEQUENCE-MISMATCH-001"
            ) {
                ExitCode::PartialFailure
            } else if code.starts_with("KIO-E-LEDGER-") || code == "KIO-E-STORE-CONSTRAINT-001" {
                ExitCode::PermanentFailure
            } else {
                ExitCode::Failure
            };
            KioError::new(code, message, json!({}), exit)
        }
        other => KioError::schema(other.to_string()),
    }
}

fn diagnostic_safe_text(input: &str, allow_newlines: bool) -> String {
    let mut output = String::with_capacity(input.len());
    for ch in input.chars() {
        if allow_newlines && ch == '\n' {
            output.push(ch);
            continue;
        }
        let code = ch as u32;
        let terminal_active = matches!(code, 0x00..=0x1f | 0x7f..=0x9f)
            || matches!(code, 0x061c | 0x200e..=0x200f | 0x2028..=0x202e | 0x2066..=0x2069);
        if terminal_active {
            if code <= 0xff {
                output.push_str(&format!("\\x{code:02x}"));
            } else {
                output.push_str(&format!("\\u{{{code:04x}}}"));
            }
        } else {
            output.push(ch);
        }
    }
    output
}

#[cfg(test)]
mod terminal_embedding_cleanup_tests;

#[cfg(test)]
mod batch_recovery_binding_tests;

#[cfg(test)]
mod network_approval_tests;

#[cfg(test)]
mod batch_ocr_tests;

#[cfg(test)]
mod tests;

#[cfg(all(test, debug_assertions))]
mod control_snapshot_tests {
    use super::*;
    use kio_core::test_control::{DebugTestControl, TestEnvGuard, test_env_lock};

    #[test]
    fn search_final_consent_barrier_uses_entry_snapshot() {
        let _lock = test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.ready");
        let second = directory.path().join("second.ready");
        fs::write(first.with_extension("release"), b"release").unwrap();
        fs::write(second.with_extension("release"), b"release").unwrap();
        let _first = TestEnvGuard::set("KIO_TEST_SEARCH_FINAL_CONSENT_BARRIER_READY", &first);
        let control = ControlHandle::new(DebugTestControl::from_env());
        let _second = TestEnvGuard::set("KIO_TEST_SEARCH_FINAL_CONSENT_BARRIER_READY", &second);
        maybe_wait_at_test_search_final_consent_barrier(&control);
        assert!(first.exists(), "the entry snapshot owns the ready marker");
        assert!(
            !second.exists(),
            "a later environment value must not replace the owner snapshot"
        );
    }
}
