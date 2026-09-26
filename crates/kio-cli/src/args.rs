//! Clap-facing command-line syntax and conversion into application requests.

use std::path::PathBuf;

use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use kio_app::commands as app;

#[derive(Debug, Parser)]
#[command(name = "kio", version, about = "Local-first knowledge archive CLI")]
pub(crate) struct Cli {
    /// Emit machine-readable JSON (docs/06-cli-spec.md §4).
    #[arg(long, global = true)]
    pub(crate) json: bool,
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Create a `.kio` directory for a folder.
    Init(InitArgs),
    /// Explicitly re-register a moved existing root.
    Root(RootArgs),
    /// Show file state, pending tasks, and budget.
    Status,
    /// Create a manual snapshot or run automatic snapshotting.
    Snapshot(SnapshotArgs),
    /// Show snapshot history.
    Log(LogArgs),
    /// Compare two snapshots.
    Diff(DiffArgs),
    /// Inspect an object by hash.
    Inspect(InspectArgs),
    /// Create or show a tag.
    Tag(TagArgs),
    /// Ingest and normalize files in the current scope.
    Index(IndexArgs),
    /// Watch an explicitly managed root and reconcile changes locally.
    Watch(WatchArgs),
    /// Resume or retry batch tasks.
    Batch(BatchArgs),
    Adapter(AdapterArgs),
    Ledger(LedgerArgs),
    Repair(RepairArgs),
    /// Plan retention-based shallow GC without changing the store.
    Gc(GcArgs),
    /// Search indexed chunks.
    Search(SearchArgs),
    /// Open an Evidence Pointer target.
    Open(PointerArgs),
    /// View an Evidence Pointer target.
    View(PointerArgs),
    /// Export historical raw bytes to an explicit destination.
    Export(ExportArgs),
    /// Restore an ancestor into the current managed working tree as a new linear commit.
    Restore(RestoreArgs),
    /// Remove content from KIO-managed history after confirmation.
    Purge(PurgeArgs),
    /// Reindex normalized instances.
    Reindex(ReindexArgs),
    /// Verify or retarget an Evidence Pointer.
    Evidence(EvidenceArgs),
}

#[derive(Debug, Args)]
pub(crate) struct InitArgs {
    pub(crate) path: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub(crate) struct RootArgs {
    #[command(subcommand)]
    pub(crate) command: RootCommand,
}
#[derive(Debug, Subcommand)]
pub(crate) enum RootCommand {
    /// Rebind an existing moved scope and its enrolled descendants; revoke device grants.
    Register(RootRegisterArgs),
}
#[derive(Debug, Args)]
#[command(group(ArgGroup::new("root_register_confirmation").required(true).multiple(false).args(["preview", "yes"])))]
pub(crate) struct RootRegisterArgs {
    /// Current directory of the moved scope (defaults to the working directory).
    pub(crate) path: Option<PathBuf>,
    /// Inspect the affected scopes and any detachment without changing them.
    #[arg(long)]
    pub(crate) preview: bool,
    /// Confirm registration and revocation of existing device grants.
    #[arg(long)]
    pub(crate) yes: bool,
    /// Resume the pending journal with its exact operation ID.
    #[arg(long, value_name = "OPERATION_ID", requires = "yes")]
    pub(crate) resume: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct WatchArgs {
    /// Explicitly registered root (defaults to the current directory).
    #[arg(long, global = true, value_name = "DIRECTORY")]
    pub(crate) root: Option<PathBuf>,
    #[command(subcommand)]
    pub(crate) command: WatchCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum WatchCommand {
    /// Run in the foreground; uses existing local adapters and grants no egress permission.
    Run {
        /// Maximum interval between full content reconciliations, in seconds.
        #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(1..=86400))]
        reconcile_interval_seconds: u64,
    },
    /// Show the instance, backend health, and pending work without starting it.
    Status,
    /// Request a graceful stop after the current reconciliation completes.
    Stop,
    /// Install or control this user's native watch scheduler registration.
    Service(WatchServiceArgs),
}

#[derive(Debug, Args)]
pub(crate) struct WatchServiceArgs {
    #[command(subcommand)]
    pub(crate) command: WatchServiceCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum WatchServiceCommand {
    /// Register a disabled-at-install-time per-user service.
    Install {
        #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(1..=86400))]
        reconcile_interval_seconds: u64,
    },
    Start,
    Stop,
    Status,
    Uninstall,
    #[command(hide = true)]
    Runner {
        #[arg(long, value_name = "ABSOLUTE_MANIFEST")]
        manifest: PathBuf,
    },
}
#[derive(Debug, Args)]
pub(crate) struct SnapshotArgs {
    #[command(subcommand)]
    pub(crate) action: SnapshotAction,
}
#[derive(Debug, Subcommand)]
pub(crate) enum SnapshotAction {
    Create(SnapshotCreateArgs),
    Auto,
}
#[derive(Debug, Args)]
pub(crate) struct SnapshotCreateArgs {
    #[arg(short, long)]
    pub(crate) message: Option<String>,
}
#[derive(Debug, Args)]
pub(crate) struct LogArgs {
    #[arg(long)]
    pub(crate) at: Option<String>,
    #[arg(long)]
    pub(crate) since: Option<String>,
}
#[derive(Debug, Args)]
pub(crate) struct DiffArgs {
    pub(crate) a: String,
    pub(crate) b: String,
}
#[derive(Debug, Args)]
pub(crate) struct InspectArgs {
    pub(crate) hash: String,
}
#[derive(Debug, Args)]
pub(crate) struct TagArgs {
    pub(crate) name: String,
    pub(crate) commit: Option<String>,
}
#[derive(Debug, Args)]
pub(crate) struct ExportArgs {
    pub(crate) source: String,
    #[arg(long)]
    pub(crate) to: PathBuf,
    #[arg(long)]
    pub(crate) force: bool,
    #[arg(long)]
    pub(crate) yes: bool,
}
#[derive(Debug, Args)]
pub(crate) struct RestoreArgs {
    /// Source commit operand: HEAD, a full commit hash, or a tag.
    pub(crate) source: String,
    /// Restore only this managed relative path. Repeat to select multiple paths.
    #[arg(long = "path", value_name = "FILE")]
    pub(crate) paths: Vec<PathBuf>,
    /// Explicitly delete this path when it is absent from the source. Repeat as needed.
    #[arg(long = "delete-missing", value_name = "FILE")]
    pub(crate) delete_missing: Vec<PathBuf>,
    /// Refuse if the current HEAD differs from this commit hash.
    #[arg(long, value_name = "HASH")]
    pub(crate) expected_head: Option<String>,
    /// Compute the exact restore plan without changing files, refs, or indexes.
    #[arg(long)]
    pub(crate) preview: bool,
    #[arg(long, value_name = "TEXT")]
    pub(crate) message: Option<String>,
}
#[derive(Debug, Args)]
pub(crate) struct GcArgs {
    #[arg(long, conflicts_with = "yes")]
    pub(crate) dry_run: bool,
    #[arg(long, conflicts_with = "dry_run")]
    pub(crate) yes: bool,
    #[arg(long, requires = "dry_run", conflicts_with = "yes")]
    pub(crate) prune_unreachable: bool,
}
#[derive(Debug, Clone, Args)]
pub(crate) struct IndexArgs {
    #[arg(long)]
    pub(crate) preview: bool,
    #[arg(long)]
    pub(crate) yes: bool,
    #[arg(long)]
    pub(crate) online: bool,
    #[arg(long)]
    pub(crate) offline: bool,
    #[arg(long, conflicts_with = "batch")]
    pub(crate) realtime: bool,
    #[arg(long)]
    pub(crate) batch: bool,
}
#[derive(Debug, Args)]
pub(crate) struct BatchArgs {
    #[command(subcommand)]
    pub(crate) command: Option<BatchCommand>,
}
#[derive(Debug, Subcommand)]
pub(crate) enum BatchCommand {
    Resume(ResumeArgs),
    Retry(RetryArgs),
    Abandon(AbandonArgs),
}
#[derive(Debug, Args)]
pub(crate) struct ResumeArgs {
    #[arg(long, conflicts_with = "override_budget")]
    pub(crate) recheck_budget: bool,
    #[arg(long)]
    pub(crate) override_budget: bool,
    #[arg(long)]
    pub(crate) online: bool,
    #[arg(long)]
    pub(crate) offline: bool,
    #[arg(long, conflicts_with = "batch")]
    pub(crate) realtime: bool,
    #[arg(long)]
    pub(crate) batch: bool,
}
#[derive(Debug, Args)]
pub(crate) struct RetryArgs {
    #[arg(
        long,
        value_name = "SELECTOR",
        conflicts_with = "resend_unknown",
        group = "retry_authorization",
        requires = "yes"
    )]
    pub(crate) reset_violations: Option<String>,
    /// Explicitly authorize one fresh send after a prior sync result was unknown.
    /// This command only queues the exact task; a later online resume performs
    /// a newly charged request after current policy and budget checks.
    #[arg(
        long,
        value_name = "SELECTOR",
        conflicts_with = "reset_violations",
        group = "retry_authorization",
        requires = "yes"
    )]
    pub(crate) resend_unknown: Option<String>,
    /// Confirm `--reset-violations` or `--resend-unknown`.
    #[arg(long, requires = "retry_authorization")]
    pub(crate) yes: bool,
    #[arg(long, conflicts_with_all = ["reset_violations", "resend_unknown"])]
    pub(crate) online: bool,
    #[arg(long, conflicts_with_all = ["reset_violations", "resend_unknown"])]
    pub(crate) offline: bool,
    #[arg(long, conflicts_with_all = ["batch", "reset_violations", "resend_unknown"])]
    pub(crate) realtime: bool,
    #[arg(long, conflicts_with_all = ["reset_violations", "resend_unknown"])]
    pub(crate) batch: bool,
}
#[derive(Debug, Args)]
pub(crate) struct AbandonArgs {
    pub(crate) selector: String,
    /// Skip the interactive abandon confirmation.
    #[arg(long)]
    pub(crate) yes: bool,
}
#[derive(Debug, Args)]
pub(crate) struct AdapterArgs {
    #[command(subcommand)]
    pub(crate) command: Option<AdapterCommand>,
}
#[derive(Debug, Subcommand)]
pub(crate) enum AdapterCommand {
    /// Approve the current adapter destination and processing profile for this scope.
    Approve(ApproveArgs),
    /// Revoke future sends for one adapter or every adapter in this scope.
    Revoke(RevokeArgs),
    /// Inspect current approvals without modifying scope or device state.
    Status(AdapterStatusArgs),
    /// Manage the device-local CA used for authenticated offline peers.
    Trust(TrustArgs),
}
#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
pub(crate) struct AdapterTargetArgs {
    pub(crate) tool_id: Option<String>,
    #[arg(long)]
    pub(crate) all: bool,
}
#[derive(Debug, Args)]
pub(crate) struct ApproveArgs {
    #[command(flatten)]
    pub(crate) target: AdapterTargetArgs,
    /// Show exact destinations, profiles and permissions without granting them.
    #[arg(long, conflicts_with = "yes")]
    pub(crate) preview: bool,
    /// Confirm the displayed explicit grant without prompting.
    #[arg(long)]
    pub(crate) yes: bool,
    /// Resume this exact pending grant; revoked grants cannot be resumed.
    #[arg(long, value_name = "GRANT_ID", conflicts_with = "all")]
    pub(crate) resume: Option<String>,
    /// Also authorize candidate-secret content for the selected adapters.
    #[arg(long)]
    pub(crate) send_secrets: bool,
}
#[derive(Debug, Args)]
pub(crate) struct RevokeArgs {
    #[command(flatten)]
    pub(crate) target: AdapterTargetArgs,
}
#[derive(Debug, Args)]
pub(crate) struct AdapterStatusArgs {
    pub(crate) tool_id: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct TrustArgs {
    #[command(subcommand)]
    pub(crate) command: TrustCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum TrustCommand {
    /// Import the first CA into device-private managed storage.
    Register(TrustRegisterArgs),
    /// Replace an active CA with a distinct managed generation.
    Rotate(TrustRotateArgs),
    /// Persist a local revocation tombstone.
    Revoke,
    /// Show local trust state without creating or repairing files.
    Status,
}

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("trust_register_confirmation").required(true).multiple(false).args(["preview", "yes"])))]
pub(crate) struct TrustRegisterArgs {
    #[arg(long, value_name = "ABSOLUTE_PEM")]
    pub(crate) ca_pem: PathBuf,
    /// Validate and show the next managed generation without writing state.
    #[arg(long)]
    pub(crate) preview: bool,
    /// Confirm importing this CA into device-private managed storage.
    #[arg(long)]
    pub(crate) yes: bool,
    /// Resume this exact interrupted registration intent.
    #[arg(long, value_name = "REGISTRATION_ID")]
    pub(crate) resume: Option<String>,
}

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("trust_rotate_confirmation").required(true).multiple(false).args(["preview", "yes"])))]
pub(crate) struct TrustRotateArgs {
    #[arg(long, value_name = "ABSOLUTE_PEM")]
    pub(crate) ca_pem: PathBuf,
    /// Validate and show the next managed generation without writing state.
    #[arg(long)]
    pub(crate) preview: bool,
    /// Confirm replacing the active managed CA.
    #[arg(long)]
    pub(crate) yes: bool,
}
#[derive(Debug, Args)]
pub(crate) struct LedgerArgs {
    #[command(subcommand)]
    pub(crate) command: Option<LedgerCommand>,
}
#[derive(Debug, Subcommand)]
pub(crate) enum LedgerCommand {
    /// Explicitly initialize a new device cost ledger. Existing state is never replaced.
    Init {
        /// Resume the exact recorded initialization after an interruption.
        #[arg(long)]
        resume: bool,
    },
    /// Inspect device ledger availability without creating or repairing it.
    Status,
    /// Create a coherent, create-only device-ledger backup at an absolute directory.
    Backup(LedgerBackupArgs),
    /// Restore a missing device ledger from an authority-bound backup directory.
    Restore(LedgerRestoreArgs),
    /// Resolve an interrupted ledger write from its exact private journal, without resetting billing history.
    Recover,
    Reconcile(ReconcileArgs),
}
#[derive(Debug, Args)]
pub(crate) struct LedgerBackupArgs {
    #[arg(long, value_name = "ABS_DIR")]
    pub(crate) to: PathBuf,
}
#[derive(Debug, Args)]
pub(crate) struct LedgerRestoreArgs {
    #[arg(long, value_name = "ABS_DIR")]
    pub(crate) from: PathBuf,
}
#[derive(Debug, Args)]
pub(crate) struct ReconcileArgs {}
#[derive(Debug, Args)]
pub(crate) struct PointerArgs {
    #[arg(value_name = "POINTER")]
    pub(crate) pointer: Option<String>,
}
#[derive(Debug, Args)]
pub(crate) struct EvidenceArgs {
    #[command(subcommand)]
    pub(crate) command: EvidenceCommand,
}
#[derive(Debug, Subcommand)]
pub(crate) enum EvidenceCommand {
    Verify(EvidenceVerifyArgs),
    Retarget(EvidenceRetargetArgs),
}
#[derive(Debug, Args)]
pub(crate) struct EvidenceVerifyArgs {
    #[arg(
        value_name = "POINTER",
        required_unless_present = "batch",
        conflicts_with = "batch"
    )]
    pub(crate) pointer: Option<String>,
    #[arg(
        long,
        value_name = "POINTERS_JSONL",
        required_unless_present = "pointer",
        conflicts_with = "pointer"
    )]
    pub(crate) batch: Option<PathBuf>,
    #[arg(long)]
    pub(crate) strict: bool,
}
#[derive(Debug, Args)]
pub(crate) struct EvidenceRetargetArgs {
    #[arg(value_name = "POINTER")]
    pub(crate) pointer: String,
    #[arg(long, value_name = "COMMIT")]
    pub(crate) at: String,
}
#[derive(Debug, Args)]
pub(crate) struct RepairArgs {
    #[command(subcommand)]
    pub(crate) operation: RepairOperation,
}
#[derive(Debug, Subcommand)]
pub(crate) enum RepairOperation {
    #[command(short_flag = 'a')]
    All,
    #[command(short_flag = 'r')]
    Replica,
    RebuildDb(RepairRebuildDbArgs),
    VerifyObjects(RepairVerifyObjectsArgs),
    RegistryPrune(RepairRegistryPruneArgs),
    /// Complete or roll back the durable journal of an interrupted managed restore.
    RecoverRestore,
    /// Inspect or cancel an unpublished child initialization in this parent scope.
    CancelChildInitialization(RepairCancelChildInitializationArgs),
}
#[derive(Debug, Args)]
pub(crate) struct RepairCancelChildInitializationArgs {
    #[arg(value_name = "PARENT_PATH")]
    pub(crate) path: Option<PathBuf>,
    #[arg(long, conflicts_with = "yes", required_unless_present = "yes")]
    pub(crate) preview: bool,
    #[arg(long, requires = "operation", required_unless_present = "preview")]
    pub(crate) yes: bool,
    #[arg(long, value_name = "OPERATION_ID")]
    pub(crate) operation: Option<String>,
}
#[derive(Debug, Args)]
pub(crate) struct RepairRebuildDbArgs {
    #[arg(long, conflicts_with = "offline")]
    pub(crate) online: bool,
    #[arg(long)]
    pub(crate) offline: bool,
    #[arg(long, conflicts_with = "batch")]
    pub(crate) realtime: bool,
    #[arg(long)]
    pub(crate) batch: bool,
}
#[derive(Debug, Args)]
pub(crate) struct RepairVerifyObjectsArgs {
    #[arg(long)]
    pub(crate) prune_orphans: bool,
    #[arg(long)]
    pub(crate) yes: bool,
}
#[derive(Debug, Args)]
pub(crate) struct RepairRegistryPruneArgs {
    #[arg(long)]
    pub(crate) yes: bool,
}
#[derive(Debug, Args)]
pub(crate) struct ReindexArgs {
    #[arg(long)]
    pub(crate) regenerate: bool,
    #[arg(long, value_name = "COMMIT", conflicts_with = "regenerate")]
    pub(crate) at: Option<String>,
    #[arg(long)]
    pub(crate) yes: bool,
    #[arg(long, conflicts_with = "offline")]
    pub(crate) online: bool,
    #[arg(long)]
    pub(crate) offline: bool,
    #[arg(long, conflicts_with = "batch")]
    pub(crate) realtime: bool,
    #[arg(long)]
    pub(crate) batch: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum SearchModeArg {
    Auto,
    Text,
    Vector,
    Hybrid,
}
#[derive(Debug, Args)]
pub(crate) struct SearchArgs {
    #[arg(value_name = "QUERY")]
    pub(crate) query: Option<String>,
    #[arg(long, value_name = "MODE", value_enum)]
    pub(crate) mode: Option<SearchModeArg>,
    #[arg(long, value_name = "PATH", conflicts_with = "all_scopes")]
    pub(crate) scope: Option<PathBuf>,
    #[arg(long, conflicts_with = "all_scopes")]
    pub(crate) descendants: bool,
    #[arg(long, conflicts_with_all = ["scope", "descendants"])]
    pub(crate) all_scopes: bool,
    #[arg(long, value_name = "COMMIT")]
    pub(crate) at: Option<String>,
    #[arg(long)]
    pub(crate) all_history: bool,
    #[arg(long)]
    pub(crate) include_deleted: bool,
    #[arg(long, value_name = "DURATION")]
    pub(crate) since: Option<String>,
    #[arg(long, value_name = "N", default_value_t = 20)]
    pub(crate) limit: u64,
    #[arg(long, value_name = "N")]
    pub(crate) offset: Option<u64>,
    #[arg(long, value_name = "TOKEN")]
    pub(crate) cursor: Option<String>,
    #[arg(long, conflicts_with = "offline")]
    pub(crate) online: bool,
    #[arg(long)]
    pub(crate) offline: bool,
}
#[derive(Debug, Args)]
#[command(group(ArgGroup::new("target").required(true).multiple(false).args(["path", "raw_hash"])))]
pub(crate) struct PurgeArgs {
    pub(crate) path: Option<PathBuf>,
    #[arg(long, value_name = "SHA256")]
    pub(crate) raw_hash: Option<String>,
    #[arg(long, value_parser = ["legal", "privacy", "misingest", "copyright", "other"])]
    pub(crate) reason: String,
    #[arg(long)]
    pub(crate) erase_tombstone: bool,
    #[arg(long)]
    pub(crate) yes: bool,
}

macro_rules! plain_from {
    ($cli:ident => $app:ident { $($field:ident),* $(,)? }) => {
        impl From<$cli> for app::$app { fn from(value: $cli) -> Self { let $cli { $($field),* } = value; Self { $($field),* } } }
    };
}
plain_from!(InitArgs => InitArgs { path });

impl From<WatchArgs> for app::WatchArgs {
    fn from(value: WatchArgs) -> Self {
        Self {
            root: value.root,
            command: match value.command {
                WatchCommand::Run {
                    reconcile_interval_seconds,
                } => app::WatchCommand::Run {
                    reconcile_interval_seconds,
                },
                WatchCommand::Status => app::WatchCommand::Status,
                WatchCommand::Stop => app::WatchCommand::Stop,
                WatchCommand::Service(service) => {
                    app::WatchCommand::Service(match service.command {
                        WatchServiceCommand::Install {
                            reconcile_interval_seconds,
                        } => app::WatchServiceCommand::Install {
                            reconcile_interval_seconds,
                        },
                        WatchServiceCommand::Start => app::WatchServiceCommand::Start,
                        WatchServiceCommand::Stop => app::WatchServiceCommand::Stop,
                        WatchServiceCommand::Status => app::WatchServiceCommand::Status,
                        WatchServiceCommand::Uninstall => app::WatchServiceCommand::Uninstall,
                        WatchServiceCommand::Runner { manifest } => {
                            app::WatchServiceCommand::Runner { manifest }
                        }
                    })
                }
            },
        }
    }
}
plain_from!(SnapshotCreateArgs => SnapshotCreateArgs { message });
plain_from!(LogArgs => LogArgs { at, since });
plain_from!(DiffArgs => DiffArgs { a, b });
plain_from!(InspectArgs => InspectArgs { hash });
plain_from!(TagArgs => TagArgs { name, commit });
plain_from!(ExportArgs => ExportArgs { source, to, force, yes });
plain_from!(RestoreArgs => RestoreArgs { source, paths, delete_missing, expected_head, preview, message });
plain_from!(GcArgs => GcArgs { dry_run, yes, prune_unreachable });
plain_from!(IndexArgs => IndexArgs { preview, yes, online, offline, realtime, batch });
plain_from!(ResumeArgs => ResumeArgs { recheck_budget, override_budget, online, offline, realtime, batch });
plain_from!(RetryArgs => RetryArgs { reset_violations, resend_unknown, yes, online, offline, realtime, batch });
plain_from!(AbandonArgs => AbandonArgs { selector, yes });
plain_from!(AdapterStatusArgs => AdapterStatusArgs { tool_id });
impl From<AdapterTargetArgs> for app::AdapterTarget {
    fn from(value: AdapterTargetArgs) -> Self {
        match value.tool_id {
            Some(tool_id) => Self::Tool(tool_id),
            None => Self::All,
        }
    }
}
impl From<ApproveArgs> for app::ApproveArgs {
    fn from(value: ApproveArgs) -> Self {
        Self {
            target: value.target.into(),
            preview: value.preview,
            yes: value.yes,
            resume: value.resume,
            send_secrets: value.send_secrets,
        }
    }
}
impl From<RevokeArgs> for app::RevokeArgs {
    fn from(value: RevokeArgs) -> Self {
        Self {
            target: value.target.into(),
        }
    }
}
plain_from!(ReconcileArgs => ReconcileArgs {});
plain_from!(PointerArgs => PointerArgs { pointer });
plain_from!(EvidenceVerifyArgs => EvidenceVerifyArgs { pointer, batch, strict });
plain_from!(EvidenceRetargetArgs => EvidenceRetargetArgs { pointer, at });
plain_from!(RepairRebuildDbArgs => RepairRebuildDbArgs { online, offline, realtime, batch });
plain_from!(RepairVerifyObjectsArgs => RepairVerifyObjectsArgs { prune_orphans, yes });
plain_from!(RepairRegistryPruneArgs => RepairRegistryPruneArgs { yes });
plain_from!(RepairCancelChildInitializationArgs => RepairCancelChildInitializationArgs { path, preview, yes, operation });
plain_from!(ReindexArgs => ReindexArgs { regenerate, at, yes, online, offline, realtime, batch });
plain_from!(PurgeArgs => PurgeArgs { path, raw_hash, reason, erase_tombstone, yes });

impl From<SnapshotAction> for app::SnapshotAction {
    fn from(value: SnapshotAction) -> Self {
        match value {
            SnapshotAction::Create(args) => Self::Create(args.into()),
            SnapshotAction::Auto => Self::Auto,
        }
    }
}
impl From<SnapshotArgs> for app::SnapshotArgs {
    fn from(value: SnapshotArgs) -> Self {
        Self {
            action: value.action.into(),
        }
    }
}
impl From<BatchCommand> for app::BatchCommand {
    fn from(value: BatchCommand) -> Self {
        match value {
            BatchCommand::Resume(args) => Self::Resume(args.into()),
            BatchCommand::Retry(args) => Self::Retry(args.into()),
            BatchCommand::Abandon(args) => Self::Abandon(args.into()),
        }
    }
}
impl From<BatchArgs> for app::BatchArgs {
    fn from(value: BatchArgs) -> Self {
        Self {
            command: value.command.map(Into::into),
        }
    }
}
impl From<TrustRegisterArgs> for app::TrustRegisterArgs {
    fn from(value: TrustRegisterArgs) -> Self {
        Self {
            ca_pem: value.ca_pem,
            preview: value.preview,
            yes: value.yes,
            resume: value.resume,
        }
    }
}
impl From<TrustRotateArgs> for app::TrustRotateArgs {
    fn from(value: TrustRotateArgs) -> Self {
        Self {
            ca_pem: value.ca_pem,
            preview: value.preview,
            yes: value.yes,
        }
    }
}
impl From<TrustCommand> for app::TrustCommand {
    fn from(value: TrustCommand) -> Self {
        match value {
            TrustCommand::Register(args) => Self::Register(args.into()),
            TrustCommand::Rotate(args) => Self::Rotate(args.into()),
            TrustCommand::Revoke => Self::Revoke,
            TrustCommand::Status => Self::Status,
        }
    }
}
impl From<TrustArgs> for app::TrustCommand {
    fn from(value: TrustArgs) -> Self {
        value.command.into()
    }
}
impl From<RootRegisterArgs> for app::RootRegisterArgs {
    fn from(value: RootRegisterArgs) -> Self {
        Self {
            path: value.path,
            preview: value.preview,
            yes: value.yes,
            resume: value.resume,
        }
    }
}
impl From<RootCommand> for app::CommandRequest {
    fn from(value: RootCommand) -> Self {
        match value {
            RootCommand::Register(args) => Self::RootRegister(args.into()),
        }
    }
}
impl From<RootArgs> for app::CommandRequest {
    fn from(value: RootArgs) -> Self {
        value.command.into()
    }
}
impl From<AdapterCommand> for app::AdapterCommand {
    fn from(value: AdapterCommand) -> Self {
        match value {
            AdapterCommand::Approve(args) => Self::Approve(args.into()),
            AdapterCommand::Revoke(args) => Self::Revoke(args.into()),
            AdapterCommand::Status(args) => Self::Status(args.into()),
            AdapterCommand::Trust(args) => Self::Trust(args.into()),
        }
    }
}
impl From<AdapterArgs> for app::AdapterArgs {
    fn from(value: AdapterArgs) -> Self {
        Self {
            command: value.command.map(Into::into),
        }
    }
}
impl From<LedgerCommand> for app::LedgerCommand {
    fn from(value: LedgerCommand) -> Self {
        match value {
            LedgerCommand::Init { resume } => Self::Init { resume },
            LedgerCommand::Status => Self::Status,
            LedgerCommand::Backup(args) => Self::Backup(args.into()),
            LedgerCommand::Restore(args) => Self::Restore(args.into()),
            LedgerCommand::Recover => Self::Recover,
            LedgerCommand::Reconcile(args) => Self::Reconcile(args.into()),
        }
    }
}
impl From<LedgerBackupArgs> for app::LedgerBackupArgs {
    fn from(value: LedgerBackupArgs) -> Self {
        Self { to: value.to }
    }
}
impl From<LedgerRestoreArgs> for app::LedgerRestoreArgs {
    fn from(value: LedgerRestoreArgs) -> Self {
        Self { from: value.from }
    }
}
impl From<LedgerArgs> for app::LedgerArgs {
    fn from(value: LedgerArgs) -> Self {
        Self {
            command: value.command.map(Into::into),
        }
    }
}
impl From<EvidenceCommand> for app::EvidenceCommand {
    fn from(value: EvidenceCommand) -> Self {
        match value {
            EvidenceCommand::Verify(args) => Self::Verify(args.into()),
            EvidenceCommand::Retarget(args) => Self::Retarget(args.into()),
        }
    }
}
impl From<EvidenceArgs> for app::EvidenceArgs {
    fn from(value: EvidenceArgs) -> Self {
        Self {
            command: value.command.into(),
        }
    }
}
impl From<RepairOperation> for app::RepairOperation {
    fn from(value: RepairOperation) -> Self {
        match value {
            RepairOperation::All => Self::All,
            RepairOperation::Replica => Self::Replica,
            RepairOperation::RebuildDb(args) => Self::RebuildDb(args.into()),
            RepairOperation::VerifyObjects(args) => Self::VerifyObjects(args.into()),
            RepairOperation::RegistryPrune(args) => Self::RegistryPrune(args.into()),
            RepairOperation::RecoverRestore => Self::RecoverRestore,
            RepairOperation::CancelChildInitialization(args) => {
                Self::CancelChildInitialization(args.into())
            }
        }
    }
}
impl From<RepairArgs> for app::RepairArgs {
    fn from(value: RepairArgs) -> Self {
        Self {
            operation: value.operation.into(),
        }
    }
}
impl From<SearchModeArg> for app::SearchMode {
    fn from(value: SearchModeArg) -> Self {
        match value {
            SearchModeArg::Auto => Self::Auto,
            SearchModeArg::Text => Self::Text,
            SearchModeArg::Vector => Self::Vector,
            SearchModeArg::Hybrid => Self::Hybrid,
        }
    }
}
impl From<SearchArgs> for app::SearchArgs {
    fn from(value: SearchArgs) -> Self {
        Self {
            query: value.query,
            mode: value.mode.map(Into::into),
            scope: value.scope,
            descendants: value.descendants,
            all_scopes: value.all_scopes,
            at: value.at,
            all_history: value.all_history,
            include_deleted: value.include_deleted,
            since: value.since,
            limit: value.limit,
            offset: value.offset,
            cursor: value.cursor,
            online: value.online,
            offline: value.offline,
        }
    }
}
impl From<Command> for app::CommandRequest {
    fn from(value: Command) -> Self {
        match value {
            Command::Init(args) => Self::Init(args.into()),
            Command::Root(args) => args.into(),
            Command::Status => Self::Status,
            Command::Snapshot(args) => Self::Snapshot(args.into()),
            Command::Log(args) => Self::Log(args.into()),
            Command::Diff(args) => Self::Diff(args.into()),
            Command::Inspect(args) => Self::Inspect(args.into()),
            Command::Tag(args) => Self::Tag(args.into()),
            Command::Index(args) => Self::Index(args.into()),
            Command::Watch(args) => Self::Watch(args.into()),
            Command::Batch(args) => Self::Batch(args.into()),
            Command::Adapter(args) => Self::Adapter(args.into()),
            Command::Ledger(args) => Self::Ledger(args.into()),
            Command::Repair(args) => Self::Repair(args.into()),
            Command::Gc(args) => Self::Gc(args.into()),
            Command::Search(args) => Self::Search(args.into()),
            Command::Open(args) => Self::Open(args.into()),
            Command::View(args) => Self::View(args.into()),
            Command::Export(args) => Self::Export(args.into()),
            Command::Restore(args) => Self::Restore(args.into()),
            Command::Purge(args) => Self::Purge(args.into()),
            Command::Reindex(args) => Self::Reindex(args.into()),
            Command::Evidence(args) => Self::Evidence(args.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{
        AdapterCommand, Cli, Command, GcArgs, RepairOperation, SnapshotAction, SnapshotArgs,
        TrustCommand, WatchCommand,
    };

    #[test]
    fn trust_lifecycle_requires_explicit_register_or_rotate_confirmation() {
        assert!(
            Cli::try_parse_from([
                "kio",
                "adapter",
                "trust",
                "register",
                "--ca-pem",
                "/private/ca.pem"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "kio",
                "adapter",
                "trust",
                "rotate",
                "--ca-pem",
                "/private/ca.pem",
                "--yes",
                "--preview"
            ])
            .is_err()
        );
        assert!(matches!(
            Cli::try_parse_from([
                "kio", "adapter", "trust", "register", "--ca-pem", "/private/ca.pem", "--preview"
            ]).unwrap().command,
            Command::Adapter(args) if matches!(args.command, Some(AdapterCommand::Trust(super::TrustArgs { command: TrustCommand::Register(_) })))
        ));
        assert!(matches!(
            Cli::try_parse_from(["kio", "adapter", "trust", "status"]).unwrap().command,
            Command::Adapter(args) if matches!(args.command, Some(AdapterCommand::Trust(super::TrustArgs { command: TrustCommand::Status })))
        ));
    }

    #[test]
    fn search_rejects_removed_mode_flags_and_conflicting_scope_selectors() {
        for removed in ["--text", "--vector", "--hybrid", "--no-vector"] {
            assert!(Cli::try_parse_from(["kio", "search", "q", removed]).is_err());
        }
        assert!(Cli::try_parse_from(["kio", "search", "q", "--mode", "bogus"]).is_err());
        assert!(
            Cli::try_parse_from(["kio", "search", "q", "--all-scopes", "--scope", ".",]).is_err()
        );
        assert!(
            Cli::try_parse_from(["kio", "search", "q", "--all-scopes", "--descendants",]).is_err()
        );
    }

    #[test]
    fn child_initialization_cancellation_requires_an_explicit_mode_and_operation() {
        for arguments in [
            vec!["kio", "repair", "cancel-child-initialization"],
            vec!["kio", "repair", "cancel-child-initialization", "--yes"],
            vec![
                "kio",
                "repair",
                "cancel-child-initialization",
                "--operation",
                "op",
            ],
            vec![
                "kio",
                "repair",
                "cancel-child-initialization",
                "--preview",
                "--yes",
                "--operation",
                "op",
            ],
        ] {
            assert!(Cli::try_parse_from(arguments).is_err());
        }
        assert!(matches!(
            Cli::try_parse_from(["kio", "repair", "cancel-child-initialization", "--preview"])
                .unwrap().command,
            Command::Repair(args) if matches!(args.operation, RepairOperation::CancelChildInitialization(_))
        ));
        assert!(matches!(
            Cli::try_parse_from(["kio", "repair", "cancel-child-initialization", "parent", "--yes", "--operation", "01ARZ3NDEKTSV4RRFFQ69G5FAV"])
                .unwrap().command,
            Command::Repair(args) if matches!(args.operation, RepairOperation::CancelChildInitialization(ref cancel)
                if cancel.yes && cancel.path.as_deref() == Some(std::path::Path::new("parent")))
        ));
    }

    #[test]
    fn parser_preserves_reindex_repair_batch_and_gc_contracts() {
        assert!(matches!(
            Cli::try_parse_from(["kio", "reindex", "--regenerate"])
                .unwrap()
                .command,
            Command::Reindex(args) if args.regenerate
        ));
        assert!(Cli::try_parse_from(["kio", "reindex", "--force"]).is_err());
        assert!(Cli::try_parse_from(["kio", "repair"]).is_err());
        assert!(Cli::try_parse_from(["kio", "repair", "--rebuild-db"]).is_err());
        assert!(matches!(
            Cli::try_parse_from(["kio", "repair", "all"]).unwrap().command,
            Command::Repair(args) if matches!(args.operation, RepairOperation::All)
        ));
        assert!(matches!(
            Cli::try_parse_from(["kio", "repair", "recover-restore"])
                .unwrap()
                .command,
            Command::Repair(args) if matches!(args.operation, RepairOperation::RecoverRestore)
        ));
        assert!(matches!(
            Cli::try_parse_from([
                "kio", "restore", "HEAD", "--path", "a.md", "--delete-missing", "b.md",
                "--expected-head", "abc", "--preview", "--message", "restore a",
            ])
            .unwrap()
            .command,
            Command::Restore(args) if args.paths.len() == 1
                && args.delete_missing.len() == 1
                && args.expected_head.as_deref() == Some("abc")
                && args.preview
        ));
        assert!(
            Cli::try_parse_from([
                "kio",
                "batch",
                "resume",
                "--recheck-budget",
                "--override-budget",
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["kio", "batch", "retry", "--yes"]).is_err());
        for action in ["--reset-violations", "--resend-unknown"] {
            assert!(Cli::try_parse_from(["kio", "batch", "retry", action, "token"]).is_err());
            assert!(
                Cli::try_parse_from(["kio", "batch", "retry", action, "token", "--yes"]).is_ok()
            );
        }
        assert!(
            Cli::try_parse_from([
                "kio",
                "batch",
                "retry",
                "--reset-violations",
                "a",
                "--resend-unknown",
                "b",
                "--yes",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "kio",
                "batch",
                "retry",
                "--reset-violations",
                "token",
                "--online",
            ])
            .is_err()
        );
        assert!(matches!(
            Cli::try_parse_from([
                "kio", "batch", "retry", "--reset-violations", "token", "--yes",
            ])
            .unwrap()
            .command,
            Command::Batch(args) if matches!(&args.command, Some(super::BatchCommand::Retry(retry)) if retry.yes)
        ));
        assert!(matches!(
            Cli::try_parse_from(["kio", "batch", "abandon", "token", "--yes"])
                .unwrap()
                .command,
            Command::Batch(args) if matches!(&args.command, Some(super::BatchCommand::Abandon(abandon)) if abandon.yes)
        ));
        assert!(matches!(
            Cli::try_parse_from(["kio", "gc"]).unwrap().command,
            Command::Gc(GcArgs {
                dry_run: false,
                yes: false,
                prune_unreachable: false
            })
        ));
        assert!(Cli::try_parse_from(["kio", "gc", "--prune-unreachable"]).is_err());
    }

    #[test]
    fn snapshot_and_global_json_forms_remain_stable() {
        let cli = Cli::try_parse_from(["kio", "snapshot", "create", "-m", "x"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Snapshot(SnapshotArgs { action: SnapshotAction::Create(args) })
                if args.message.as_deref() == Some("x")
        ));
        assert!(matches!(
            Cli::try_parse_from(["kio", "snapshot", "auto"])
                .unwrap()
                .command,
            Command::Snapshot(SnapshotArgs {
                action: SnapshotAction::Auto
            })
        ));
        assert!(Cli::try_parse_from(["kio", "snapshot"]).is_err());
        assert!(Cli::try_parse_from(["kio", "commit", "-m", "x"]).is_err());
        assert!(Cli::try_parse_from(["kio", "diff", "a"]).is_err());
        let cli = Cli::try_parse_from(["kio", "status", "--json"]).unwrap();
        assert!(cli.json);
        assert!(matches!(cli.command, Command::Status));
    }

    #[test]
    fn watch_service_parser_preserves_foreground_commands_and_bounds_interval() {
        assert!(matches!(
            Cli::try_parse_from(["kio", "watch", "service", "install", "--reconcile-interval-seconds", "1"])
                .unwrap()
                .command,
            Command::Watch(args) if matches!(args.command, WatchCommand::Service(_))
        ));
        assert!(
            Cli::try_parse_from([
                "kio",
                "watch",
                "service",
                "install",
                "--reconcile-interval-seconds",
                "0",
            ])
            .is_err()
        );
        assert!(matches!(
            Cli::try_parse_from(["kio", "watch", "run"]).unwrap().command,
            Command::Watch(args) if matches!(args.command, WatchCommand::Run { .. })
        ));
    }
}
