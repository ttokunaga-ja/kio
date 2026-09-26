//! Typed, debug-build-only test controls.
//!
//! This is the single production parser for `KIO_TEST_*` and `KIO_FIXED_NOW`.
//! Callers intentionally receive `None` for an unset value and
//! [`Selector::Unknown`] for a set but unsupported selector; unknown values are
//! never silently treated as an enabled mock or fault point.

use std::cell::RefCell;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use crate::durability::DurabilityPoint;

/// A finite test selector read from the environment.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Selector<T> {
    /// The environment variable was not present.
    #[default]
    Unset,
    /// The environment variable selected a supported value.
    Known(T),
    /// The variable was present, but is not a supported UTF-8 selector.
    Unknown(OsString),
}

impl<T> Selector<T> {
    pub fn is_unset(&self) -> bool {
        matches!(self, Self::Unset)
    }

    pub fn known(&self) -> Option<&T> {
        match self {
            Self::Known(value) => Some(value),
            Self::Unset | Self::Unknown(_) => None,
        }
    }
}

macro_rules! finite_selector {
    ($name:ident { $($raw:literal => $variant:ident),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub enum $name { $($variant),+ }

        impl $name {
            fn parse(value: &str) -> Option<Self> {
                match value { $($raw => Some(Self::$variant),)+ _ => None }
            }
        }
    };
}

finite_selector!(MistralOcrMode {
    "auth_error" => AuthError, "rate_limit" => RateLimit,
    "rate_limit_after" => RateLimitAfter, "network_error" => NetworkError,
    "mock" => Mock, "partial" => Partial, "partial_network" => PartialNetwork,
    "all_failed_network" => AllFailedNetwork,
    "all_failed_invalid_input" => AllFailedInvalidInput, "mock_link_image" => MockLinkImage,
    "incr_incomplete" => IncrementalIncomplete, "pin_changed" => PinChanged,
    "no_change_no_send" => NoChangeNoSend,
    "require_idempotency_token" => RequireIdempotencyToken,
});
finite_selector!(GeminiEmbedMode {
    "mock" => Mock, "incompatible_profile" => IncompatibleProfile,
    "non_multimodal" => NonMultimodal, "auth_error" => AuthError,
    "rate_limit" => RateLimit, "rate_limit_after" => RateLimitAfter,
    "network_error" => NetworkError,
    "require_idempotency_token" => RequireIdempotencyToken,
    "no_usage_report" => NoUsageReport,
});
finite_selector!(LocalOcrMode { "mock" => Mock });
finite_selector!(LocalOcrBodyMode {
    "nonconforming" => Nonconforming, "table" => Table, "decorated" => Decorated,
});
finite_selector!(MarkdownizeAdapterMode {
    "incremental" => Incremental, "reject_incremental" => RejectIncremental,
    "reject_incremental_and_full" => RejectIncrementalAndFull,
});
finite_selector!(AggregatorProjectionFault { "refresh" => Refresh });
finite_selector!(ReplicaFault {
    "index_before_marker" => IndexBeforeMarker, "index" => Index,
    "reindex_before_marker" => ReindexBeforeMarker, "reindex" => Reindex,
});
finite_selector!(GcFault {
    "after_marker_fsync" => AfterMarkerFsync,
    "after_first_receipt" => AfterFirstReceipt,
    "after_all_receipts_before_tree_delete" => AfterAllReceiptsBeforeTreeDelete,
    "after_pre_sweep_rotation" => AfterPreSweepRotation,
    "after_first_tree_delete" => AfterFirstTreeDelete,
    "after_all_trees_before_final_rotation" => AfterAllTreesBeforeFinalRotation,
    "after_final_rotation_before_marker_removal" => AfterFinalRotationBeforeMarkerRemoval,
    "after_private_prepare" => AfterPrivatePrepare,
    "after_rotation_marker_persist" => AfterRotationMarkerPersist,
    "after_index_exchange" => AfterIndexExchange,
    "after_temp_cleanup_before_marker_advance" => AfterTempCleanupBeforeMarkerAdvance,
    "after_marker_stage_fsync" => AfterMarkerStageFsync,
    "after_receipt_stage_fsync" => AfterReceiptStageFsync,
    "after_tree_quarantine" => AfterTreeQuarantine,
    "after_tree_retirement_capture" => AfterTreeRetirementCapture,
});
finite_selector!(PromotionFault {
    "before_head" => BeforeHead, "after_head" => AfterHead,
    "after_index_swap" => AfterIndexSwap,
});
finite_selector!(PurgeFault {
    "prepared" => Prepared, "prepared_visible" => PreparedVisible,
    "tombstoned" => Tombstoned, "deleted" => Deleted, "committed" => Committed,
});

fn selector<T>(name: &str, parse: impl FnOnce(&str) -> Option<T>) -> Selector<T> {
    match std::env::var_os(name) {
        None => Selector::Unset,
        Some(value) => value
            .to_str()
            .and_then(parse)
            .map(Selector::Known)
            .unwrap_or(Selector::Unknown(value)),
    }
}

fn text(name: &str) -> Selector<String> {
    match std::env::var_os(name) {
        None => Selector::Unset,
        Some(value) => match value.into_string() {
            Ok(value) => Selector::Known(value),
            Err(value) => Selector::Unknown(value),
        },
    }
}

fn path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

fn u64_selector(name: &str) -> Selector<u64> {
    selector(name, |value| value.parse().ok())
}

/// Adapter seams. Fixture values that name files retain OS-path semantics.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AdapterTestControl {
    pub mistral_ocr: Selector<MistralOcrMode>,
    pub gemini_embed: Selector<GeminiEmbedMode>,
    pub local_ocr: Selector<LocalOcrMode>,
    pub local_ocr_body: Selector<LocalOcrBodyMode>,
    pub mistral_batch: Selector<String>,
    pub gemini_batch: Selector<String>,
    pub batch_inventory: Option<PathBuf>,
    pub office_convert: Option<PathBuf>,
    pub capture_sent_media: Option<PathBuf>,
}

/// Snapshot and GC barriers used by core operations.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CoreTestControl {
    pub windows_profile: Option<PathBuf>,
    pub hold_lock_ready: Option<PathBuf>,
    pub snapshot_authority_capture_ready: Option<PathBuf>,
    pub snapshot_bound_layout_ready: Option<PathBuf>,
    pub snapshot_pre_checkpoint_ready: Option<PathBuf>,
    pub snapshot_before_state_write_ready: Option<PathBuf>,
    pub snapshot_after_state_write_ready: Option<PathBuf>,
    pub gc_pre_quarantine_ready: Option<PathBuf>,
    pub gc_tree_quarantine_ready: Option<PathBuf>,
    pub gc_fault: Selector<GcFault>,
    pub gc_index_copy_ready: Option<PathBuf>,
    pub durability_point: Selector<DurabilityPoint>,
    pub durability_ready: Option<PathBuf>,
}

/// CLI-only seams and process coordination controls.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CliTestControl {
    pub snapshot_pre_gc_preflight_ready: Option<PathBuf>,
    pub snapshot_prelock_ready: Option<PathBuf>,
    pub snapshot_locked_ready: Option<PathBuf>,
    pub snapshot_before_publication_ready: Option<PathBuf>,
    pub snapshot_writer_boundary_ready: Option<PathBuf>,
    pub aggregator_projection_fault: Selector<AggregatorProjectionFault>,
    pub replica_after_head_fault: Selector<ReplicaFault>,
    pub search_response_barrier_ready: Option<PathBuf>,
    pub query_embed_trace: Option<PathBuf>,
    pub markdownize_adapter: Selector<MarkdownizeAdapterMode>,
    pub gc_post_publication_ready: Option<PathBuf>,
    pub gc_runtime_checkpoints: Selector<u64>,
    pub gc_prelock_ready: Option<PathBuf>,
    pub promotion_fault: Selector<PromotionFault>,
    pub purge_fail_after_phase: Selector<PurgeFault>,
    pub scope_search_delay_scope_id: Selector<String>,
    pub scope_search_delay_ms: Selector<u64>,
}

/// All debug test controls observed at one operation boundary.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DebugTestControl {
    pub fixed_now: Selector<String>,
    pub adapters: AdapterTestControl,
    pub core: CoreTestControl,
    pub cli: CliTestControl,
}

thread_local! {
    static INSTALLED: RefCell<Option<DebugTestControl>> = const { RefCell::new(None) };
}

/// Install the operation snapshot used by downstream crates on this thread.
///
/// The CLI calls this exactly once after parsing arguments. Library operation
/// roots that do not run through the CLI use [`capture_for_operation`] and pass
/// the relevant typed sub-control explicitly instead.
pub fn install(control: DebugTestControl) {
    INSTALLED.with(|slot| *slot.borrow_mut() = Some(control));
}

/// Temporarily replace the installed snapshot and restore it on drop.
pub struct InstalledTestControlGuard {
    previous: Option<DebugTestControl>,
}

pub fn install_scoped(control: DebugTestControl) -> InstalledTestControlGuard {
    let previous = INSTALLED.with(|slot| slot.borrow_mut().replace(control));
    InstalledTestControlGuard { previous }
}

impl Drop for InstalledTestControlGuard {
    fn drop(&mut self) {
        INSTALLED.with(|slot| *slot.borrow_mut() = self.previous.take());
    }
}

/// Return the snapshot installed by the current composition root.
pub fn current() -> Option<DebugTestControl> {
    INSTALLED.with(|slot| slot.borrow().clone())
}

/// Capture one operation-local snapshot, reusing the composition-root value
/// when one is already installed.
pub fn capture_for_operation() -> DebugTestControl {
    current().unwrap_or_else(DebugTestControl::from_env)
}

/// Read the installed snapshot without consulting the ambient environment.
/// Downstream helpers use this so a missing composition root disables test
/// controls instead of reparsing them part-way through an operation.
pub fn current_or_default() -> DebugTestControl {
    current().unwrap_or_default()
}

/// Process-wide lock for tests that temporarily mutate environment variables.
///
/// Every crate in one test process reaches this same lock through `kio-core`,
/// avoiding module-local mutexes that cannot serialize sibling test modules.
pub fn test_env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Panic-safe restoration for a test environment variable.
pub struct TestEnvGuard {
    name: &'static str,
    previous: Option<OsString>,
}

impl TestEnvGuard {
    pub fn set(name: &'static str, value: impl AsRef<OsStr>) -> Self {
        let previous = std::env::var_os(name);
        // The caller holds `test_env_lock`; Rust 2024 marks process env
        // mutation unsafe because it is otherwise globally racy.
        unsafe { std::env::set_var(name, value) };
        Self { name, previous }
    }

    pub fn remove(name: &'static str) -> Self {
        let previous = std::env::var_os(name);
        unsafe { std::env::remove_var(name) };
        Self { name, previous }
    }
}

impl Drop for TestEnvGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => unsafe { std::env::set_var(self.name, value) },
            None => unsafe { std::env::remove_var(self.name) },
        }
    }
}

impl DebugTestControl {
    /// Parse the current environment once at an operation boundary.
    pub fn from_env() -> Self {
        Self {
            fixed_now: text("KIO_FIXED_NOW"),
            adapters: AdapterTestControl {
                mistral_ocr: selector("KIO_TEST_MISTRAL_OCR", MistralOcrMode::parse),
                gemini_embed: selector("KIO_TEST_GEMINI_EMBED", GeminiEmbedMode::parse),
                local_ocr: selector("KIO_TEST_LOCAL_OCR", LocalOcrMode::parse),
                local_ocr_body: selector("KIO_TEST_LOCAL_OCR_BODY", LocalOcrBodyMode::parse),
                mistral_batch: text("KIO_TEST_MISTRAL_BATCH"),
                gemini_batch: text("KIO_TEST_GEMINI_BATCH"),
                batch_inventory: path("KIO_TEST_BATCH_INVENTORY"),
                office_convert: path("KIO_TEST_OFFICE_CONVERT"),
                capture_sent_media: path("KIO_TEST_CAPTURE_SENT_MEDIA"),
            },
            core: CoreTestControl {
                windows_profile: path("KIO_TEST_WINDOWS_PROFILE"),
                hold_lock_ready: path("KIO_TEST_HOLD_LOCK_READY"),
                snapshot_authority_capture_ready: path(
                    "KIO_TEST_SNAPSHOT_AUTO_AUTHORITY_CAPTURE_READY",
                ),
                snapshot_bound_layout_ready: path("KIO_TEST_SNAPSHOT_AUTO_BOUND_LAYOUT_READY"),
                snapshot_pre_checkpoint_ready: path("KIO_TEST_SNAPSHOT_AUTO_PRE_CHECKPOINT_READY"),
                snapshot_before_state_write_ready: path(
                    "KIO_TEST_SNAPSHOT_AUTO_BEFORE_STATE_WRITE_READY",
                ),
                snapshot_after_state_write_ready: path(
                    "KIO_TEST_SNAPSHOT_AUTO_AFTER_STATE_WRITE_READY",
                ),
                gc_pre_quarantine_ready: path("KIO_TEST_GC_PRE_QUARANTINE_READY"),
                gc_tree_quarantine_ready: path("KIO_TEST_GC_TREE_QUARANTINE_READY"),
                gc_fault: selector("KIO_TEST_GC_FAULT", GcFault::parse),
                gc_index_copy_ready: path("KIO_TEST_GC_INDEX_COPY_READY"),
                durability_point: selector("KIO_TEST_DURABILITY_POINT", DurabilityPoint::parse),
                durability_ready: path("KIO_TEST_DURABILITY_READY"),
            },
            cli: CliTestControl {
                snapshot_pre_gc_preflight_ready: path(
                    "KIO_TEST_SNAPSHOT_AUTO_PRE_GC_PREFLIGHT_READY",
                ),
                snapshot_prelock_ready: path("KIO_TEST_SNAPSHOT_AUTO_PRELOCK_READY"),
                snapshot_locked_ready: path("KIO_TEST_SNAPSHOT_AUTO_LOCKED_READY"),
                snapshot_before_publication_ready: path(
                    "KIO_TEST_SNAPSHOT_AUTO_BEFORE_PUBLICATION_READY",
                ),
                snapshot_writer_boundary_ready: path(
                    "KIO_TEST_SNAPSHOT_AUTO_WRITER_BOUNDARY_READY",
                ),
                aggregator_projection_fault: selector(
                    "KIO_TEST_AGGREGATOR_PROJECTION_FAULT",
                    AggregatorProjectionFault::parse,
                ),
                replica_after_head_fault: selector(
                    "KIO_TEST_REPLICA_AFTER_HEAD_FAULT",
                    ReplicaFault::parse,
                ),
                search_response_barrier_ready: path("KIO_TEST_SEARCH_RESPONSE_BARRIER_READY"),
                query_embed_trace: path("KIO_TEST_QUERY_EMBED_TRACE"),
                markdownize_adapter: selector(
                    "KIO_TEST_MARKDOWNIZE_ADAPTER",
                    MarkdownizeAdapterMode::parse,
                ),
                gc_post_publication_ready: path("KIO_TEST_GC_POST_PUBLICATION_READY"),
                gc_runtime_checkpoints: u64_selector("KIO_TEST_GC_RUNTIME_CHECKPOINTS"),
                gc_prelock_ready: path("KIO_TEST_GC_PRELOCK_READY"),
                promotion_fault: selector("KIO_TEST_PROMOTION_FAULT", PromotionFault::parse),
                purge_fail_after_phase: selector(
                    "KIO_TEST_PURGE_FAIL_AFTER_PHASE",
                    PurgeFault::parse,
                ),
                scope_search_delay_scope_id: text("KIO_TEST_SCOPE_SEARCH_DELAY_SCOPE_ID"),
                scope_search_delay_ms: u64_selector("KIO_TEST_SCOPE_SEARCH_DELAY_MS"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_finite_and_path_controls() {
        let _lock = test_env_lock().lock().unwrap();
        let _ocr = TestEnvGuard::set("KIO_TEST_MISTRAL_OCR", "mock_link_image");
        let _ready = TestEnvGuard::set("KIO_TEST_GC_PRELOCK_READY", "/tmp/kio-ready");
        let _count = TestEnvGuard::set("KIO_TEST_GC_RUNTIME_CHECKPOINTS", "17");
        let control = DebugTestControl::from_env();
        assert_eq!(
            control.adapters.mistral_ocr,
            Selector::Known(MistralOcrMode::MockLinkImage)
        );
        assert_eq!(
            control.cli.gc_prelock_ready,
            Some(PathBuf::from("/tmp/kio-ready"))
        );
        assert_eq!(control.cli.gc_runtime_checkpoints, Selector::Known(17));
    }

    #[test]
    fn retains_unknown_and_unset_selectors() {
        let _lock = test_env_lock().lock().unwrap();
        let _mode = TestEnvGuard::set("KIO_TEST_GEMINI_EMBED", "future-mode");
        let _promotion = TestEnvGuard::remove("KIO_TEST_PROMOTION_FAULT");
        let control = DebugTestControl::from_env();
        assert_eq!(
            control.adapters.gemini_embed,
            Selector::Unknown(OsString::from("future-mode"))
        );
        assert!(control.cli.promotion_fault.is_unset());
    }

    #[test]
    fn fixed_now_and_scope_delay_are_text_and_numeric() {
        let _lock = test_env_lock().lock().unwrap();
        let _now = TestEnvGuard::set("KIO_FIXED_NOW", "2026-08-25T00:00:00Z");
        let _delay = TestEnvGuard::set("KIO_TEST_SCOPE_SEARCH_DELAY_MS", "not-a-number");
        let control = DebugTestControl::from_env();
        assert_eq!(
            control.fixed_now.known(),
            Some(&"2026-08-25T00:00:00Z".to_owned())
        );
        assert!(matches!(
            control.cli.scope_search_delay_ms,
            Selector::Unknown(_)
        ));
    }

    #[test]
    fn installed_snapshot_ignores_later_environment_mutation() {
        let _lock = test_env_lock().lock().unwrap();
        let _initial = TestEnvGuard::set("KIO_TEST_GC_PRELOCK_READY", "/tmp/initial-ready");
        let _installed = install_scoped(DebugTestControl::from_env());
        let _changed = TestEnvGuard::set("KIO_TEST_GC_PRELOCK_READY", "/tmp/later-ready");

        assert_eq!(
            current_or_default().cli.gc_prelock_ready,
            Some(PathBuf::from("/tmp/initial-ready"))
        );
    }

    #[test]
    fn parses_durability_control_as_a_finite_selector() {
        let _lock = test_env_lock().lock().unwrap();
        let _point = TestEnvGuard::set("KIO_TEST_DURABILITY_POINT", "publication_head");
        let _ready = TestEnvGuard::set("KIO_TEST_DURABILITY_READY", "/tmp/kio-durability-ready");
        let control = DebugTestControl::from_env();

        assert_eq!(
            control.core.durability_point,
            Selector::Known(DurabilityPoint::PublicationHead)
        );
        assert_eq!(
            control.core.durability_ready,
            Some(PathBuf::from("/tmp/kio-durability-ready"))
        );
    }
}
