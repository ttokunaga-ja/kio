//! Application command requests, independent of any command-line parser.

use std::path::PathBuf;

/// A request accepted by Kio's application layer.
#[derive(Debug, Clone)]
pub enum CommandRequest {
    /// Initialize a scope.
    Init(InitArgs),
    /// Explicitly re-register a moved existing root.
    RootRegister(RootRegisterArgs),
    /// Report a scope's current state.
    Status,
    /// Create a snapshot or run snapshot automation.
    Snapshot(SnapshotArgs),
    /// Read snapshot history.
    Log(LogArgs),
    Diff(DiffArgs),
    Inspect(InspectArgs),
    Tag(TagArgs),
    /// Index working-tree content.
    Index(IndexArgs),
    /// Run or control local filesystem reconciliation.
    Watch(WatchArgs),
    /// Resume, retry, or abandon batch work.
    Batch(BatchArgs),
    Adapter(AdapterArgs),
    Ledger(LedgerArgs),
    Repair(RepairArgs),
    Gc(GcArgs),
    /// Search indexed content.
    Search(SearchArgs),
    Open(PointerArgs),
    View(PointerArgs),
    /// Export historical raw bytes to an explicit destination.
    Export(ExportArgs),
    /// Restore selected historical content into the managed working tree.
    Restore(RestoreArgs),
    /// Permanently purge selected managed content after confirmation.
    Purge(PurgeArgs),
    Reindex(ReindexArgs),
    Evidence(EvidenceArgs),
}

#[derive(Debug, Clone)]
pub struct InitArgs {
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct RootRegisterArgs {
    pub path: Option<PathBuf>,
    pub preview: bool,
    pub yes: bool,
    pub resume: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WatchArgs {
    pub root: Option<PathBuf>,
    pub command: WatchCommand,
}

#[derive(Debug, Clone)]
pub enum WatchCommand {
    Run {
        reconcile_interval_seconds: u64,
    },
    Status,
    Stop,
    /// Install or control the current user's native scheduler registration.
    Service(WatchServiceCommand),
}

#[derive(Debug, Clone)]
pub enum WatchServiceCommand {
    Install {
        reconcile_interval_seconds: u64,
    },
    Start,
    Stop,
    Status,
    Uninstall,
    /// Native-scheduler entrypoint. The manifest binding is verified before a
    /// watcher child is started; this is not an interactive lifecycle command.
    Runner {
        manifest: PathBuf,
    },
}

#[derive(Debug, Clone)]
pub struct SnapshotArgs {
    pub action: SnapshotAction,
}

#[derive(Debug, Clone)]
pub enum SnapshotAction {
    Create(SnapshotCreateArgs),
    Auto,
}

#[derive(Debug, Clone)]
pub struct SnapshotCreateArgs {
    pub message: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LogArgs {
    pub at: Option<String>,
    pub since: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DiffArgs {
    pub a: String,
    pub b: String,
}

#[derive(Debug, Clone)]
pub struct InspectArgs {
    pub hash: String,
}

#[derive(Debug, Clone)]
pub struct TagArgs {
    pub name: String,
    pub commit: Option<String>,
    pub delete: bool,
}

#[derive(Debug, Clone)]
pub struct ExportArgs {
    pub source: String,
    pub to: PathBuf,
    pub force: bool,
    pub yes: bool,
}

/// A journaled, linear restore of an ancestor into the current managed scope.
#[derive(Debug, Clone)]
pub struct RestoreArgs {
    pub source: String,
    pub paths: Vec<PathBuf>,
    pub delete_missing: Vec<PathBuf>,
    pub expected_head: Option<String>,
    pub preview: bool,
    pub message: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GcArgs {
    pub dry_run: bool,
    pub yes: bool,
    pub prune_unreachable: bool,
}

#[derive(Debug, Clone)]
pub struct IndexArgs {
    pub preview: bool,
    pub yes: bool,
    pub online: bool,
    pub offline: bool,
    pub realtime: bool,
    pub batch: bool,
}

#[derive(Debug, Clone)]
pub struct BatchArgs {
    pub command: Option<BatchCommand>,
}

#[derive(Debug, Clone)]
pub enum BatchCommand {
    Resume(ResumeArgs),
    Retry(RetryArgs),
    Abandon(AbandonArgs),
}

#[derive(Debug, Clone)]
pub struct ResumeArgs {
    pub recheck_budget: bool,
    pub override_budget: bool,
    pub online: bool,
    pub offline: bool,
    pub realtime: bool,
    pub batch: bool,
}

#[derive(Debug, Clone)]
pub struct RetryArgs {
    pub reset_violations: Option<String>,
    /// Explicitly authorize one fresh send after a prior sync result was unknown.
    pub resend_unknown: Option<String>,
    pub yes: bool,
    pub online: bool,
    pub offline: bool,
    pub realtime: bool,
    pub batch: bool,
}

#[derive(Debug, Clone)]
pub struct AbandonArgs {
    pub selector: String,
    pub yes: bool,
}

#[derive(Debug, Clone)]
pub struct AdapterArgs {
    pub command: Option<AdapterCommand>,
}

#[derive(Debug, Clone)]
pub enum AdapterCommand {
    Approve(ApproveArgs),
    Revoke(RevokeArgs),
    Status(AdapterStatusArgs),
    /// Device-local offline peer trust lifecycle. This never reads a scope.
    Trust(TrustCommand),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterTarget {
    Tool(String),
    All,
}

#[derive(Debug, Clone)]
pub struct ApproveArgs {
    pub target: AdapterTarget,
    pub preview: bool,
    pub yes: bool,
    pub resume: Option<String>,
    pub send_secrets: bool,
}

#[derive(Debug, Clone)]
pub struct RevokeArgs {
    pub target: AdapterTarget,
}

#[derive(Debug, Clone)]
pub struct AdapterStatusArgs {
    pub tool_id: Option<String>,
}

#[derive(Debug, Clone)]
pub enum TrustCommand {
    Register(TrustRegisterArgs),
    Rotate(TrustRotateArgs),
    Revoke,
    Status,
}

#[derive(Debug, Clone)]
pub struct TrustRegisterArgs {
    pub ca_pem: PathBuf,
    pub preview: bool,
    pub yes: bool,
    pub resume: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TrustRotateArgs {
    pub ca_pem: PathBuf,
    pub preview: bool,
    pub yes: bool,
}

#[derive(Debug, Clone)]
pub struct LedgerArgs {
    pub command: Option<LedgerCommand>,
}

#[derive(Debug, Clone)]
pub enum LedgerCommand {
    Init { resume: bool },
    Status,
    Backup(LedgerBackupArgs),
    Restore(LedgerRestoreArgs),
    Recover,
    Reconcile(ReconcileArgs),
}

#[derive(Debug, Clone)]
pub struct LedgerBackupArgs {
    pub to: PathBuf,
}

#[derive(Debug, Clone)]
pub struct LedgerRestoreArgs {
    pub from: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ReconcileArgs {}

#[derive(Debug, Clone)]
pub struct PointerArgs {
    pub pointer: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EvidenceArgs {
    pub command: EvidenceCommand,
}

#[derive(Debug, Clone)]
pub enum EvidenceCommand {
    Verify(EvidenceVerifyArgs),
    Retarget(EvidenceRetargetArgs),
}

#[derive(Debug, Clone)]
pub struct EvidenceVerifyArgs {
    pub pointer: Option<String>,
    pub batch: Option<PathBuf>,
    pub strict: bool,
}

#[derive(Debug, Clone)]
pub struct EvidenceRetargetArgs {
    pub pointer: String,
    pub at: String,
}

#[derive(Debug, Clone)]
pub struct RepairArgs {
    pub operation: RepairOperation,
}

#[derive(Debug, Clone)]
pub enum RepairOperation {
    All,
    Replica,
    RebuildDb(RepairRebuildDbArgs),
    VerifyObjects(RepairVerifyObjectsArgs),
    RegistryPrune(RepairRegistryPruneArgs),
    /// Finish or roll back an interrupted managed working-tree restore.
    RecoverRestore,
    /// Cancel a precisely identified, unpublished child initialization.
    CancelChildInitialization(RepairCancelChildInitializationArgs),
}

#[derive(Debug, Clone)]
pub struct RepairCancelChildInitializationArgs {
    pub path: Option<PathBuf>,
    pub preview: bool,
    pub yes: bool,
    pub operation: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RepairRebuildDbArgs {
    pub online: bool,
    pub offline: bool,
    pub realtime: bool,
    pub batch: bool,
}

#[derive(Debug, Clone)]
pub struct RepairVerifyObjectsArgs {
    pub prune_orphans: bool,
    pub yes: bool,
}

#[derive(Debug, Clone)]
pub struct RepairRegistryPruneArgs {
    pub yes: bool,
}

#[derive(Debug, Clone)]
pub struct ReindexArgs {
    pub regenerate: bool,
    pub at: Option<String>,
    pub yes: bool,
    pub online: bool,
    pub offline: bool,
    pub realtime: bool,
    pub batch: bool,
}

/// Search mode requested by an application client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    Auto,
    Text,
    Vector,
    Hybrid,
}

#[derive(Debug, Clone)]
pub struct SearchArgs {
    pub query: Option<String>,
    pub mode: Option<SearchMode>,
    pub scope: Option<PathBuf>,
    pub descendants: bool,
    pub all_scopes: bool,
    pub at: Option<String>,
    pub all_history: bool,
    pub include_deleted: bool,
    pub since: Option<String>,
    pub limit: u64,
    pub offset: Option<u64>,
    pub cursor: Option<String>,
    pub online: bool,
    pub offline: bool,
}

/// Destructive purge selection and confirmation request.
#[derive(Debug, Clone)]
pub struct PurgeArgs {
    pub path: Option<PathBuf>,
    pub raw_hash: Option<String>,
    pub reason: String,
    pub erase_tombstone: bool,
    pub yes: bool,
}
