mod batch;
mod egress;
mod embedding;
mod ledger_access;
mod markdownize;
mod object_persistence;
mod task_execution;
use crate::batch::command::run_adapter;
use crate::batch::command::run_batch;
use crate::commands::AdapterArgs;
use crate::commands::AdapterCommand;
use crate::commands::CommandRequest as Command;
use crate::commands::LedgerArgs;
use crate::commands::LedgerCommand;
use crate::commands::SnapshotAction;
use crate::commands::SnapshotArgs;
use crate::context::AppContext;
use crate::index::command::run_index;
use crate::index::command::run_reindex;
use crate::invocation::Invocation;
use crate::invocation::InvocationRuntime;
use crate::invocation::execute_lifecycle;
use crate::ledger_access::run_ledger;
use crate::open_view::run_open;
use crate::open_view::run_view;
use crate::repair_command::run_repair;
use crate::response::CommandResponse;
use crate::scope_commands::run_init;
use crate::scope_commands::run_status;
use crate::search::command::run_search;
use crate::settings::adapter_runtime_settings_from_user_config;
use crate::settings::ensure_device_dirs_resolvable;
use crate::settings::validate_user_config;
use crate::settings::validate_user_tools_config;
use crate::snapshot_history::run_diff;
use crate::snapshot_history::run_inspect;
use crate::snapshot_history::run_log;
use crate::snapshot_history::run_snapshot_auto;
use crate::snapshot_history::run_snapshot_create;
use crate::snapshot_history::run_tag;
use kio_core::Result;
#[cfg(debug_assertions)]
use kio_core::test_control::DebugTestControl;
use std::sync::Arc;

mod index;
mod open_view;
mod search;

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

pub mod commands;
pub mod context;
pub mod response;

pub fn execute(ctx: &AppContext, command: Command) -> Result<CommandResponse> {
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

fn execute_dispatch(ctx: &Invocation<'_>, command: Command) -> Result<CommandResponse> {
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
mod control_snapshot_tests;
