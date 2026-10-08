//! Windows journaled no-replace GC exchange; callers hold the writer barrier.
use crate::Result;
use crate::gc::exchange::{
    BACKUP, INTENT, OperationKind, Placement, failure, inspect_pending, placement, validate_leaves,
};
use crate::store_dir::StoreDirectory;
#[cfg(debug_assertions)]
use crate::{ExitCode, KioError};
use std::path::Path;

const MAX_INTENT: u64 = 16 * 1024;
use crate::management::{DirectoryIdentity, directory_identity_from_handle};
use crate::store_dir::Publication;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileRecord {
    #[serde(with = "crate::identity_serde::u32_hex")]
    volume: u32,
    #[serde(with = "crate::identity_serde::u64_hex")]
    index: u64,
    length: u64,
    sha256: String,
}
impl FileRecord {
    pub fn identity(&self) -> (u32, u64) {
        (self.volume, self.index)
    }
    pub fn length(&self) -> u64 {
        self.length
    }
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExchangeExpected {
    pub source: FileRecord,
    pub target: FileRecord,
}

/// Hash a bounded single-link regular file using its exact retained handle.
pub fn observe(file: &File, max: u64) -> Result<FileRecord> {
    let id = crate::platform::regular_file_handle_identity(file)
        .ok_or_else(|| failure("unsafe exchange file handle"))?;
    let (volume, index) = id.atomic_recovery_components();
    let length = file.metadata().map_err(|e| failure(e.to_string()))?.len();
    if length > max {
        return Err(failure("exchange file exceeds bound"));
    }
    let mut cursor = file;
    cursor
        .seek(SeekFrom::Start(0))
        .map_err(|e| failure(e.to_string()))?;
    let mut hash = Sha256::new();
    let mut read = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = cursor
            .read(&mut buffer)
            .map_err(|e| failure(e.to_string()))?;
        if n == 0 {
            break;
        }
        read = read
            .checked_add(n as u64)
            .ok_or_else(|| failure("exchange length overflow"))?;
        if read > max || read > length {
            return Err(failure("exchange file grew while hashing"));
        }
        hash.update(&buffer[..n]);
    }
    if read != length
        || crate::platform::regular_file_handle_identity(file) != Some(id)
        || file.metadata().map_err(|e| failure(e.to_string()))?.len() != length
    {
        return Err(failure("exchange file changed while hashing"));
    }
    Ok(FileRecord {
        volume,
        index,
        length,
        sha256: crate::cas::lower_hex(&hash.finalize()),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExchangeStep {
    IntentPublished,
    SourceCaptured,
    TargetPublished,
    ExchangeComplete,
    SourceRetired,
    IntentRemoved,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    authority_binding: String,
    version: u8,
    kind: OperationKind,
    owner: DirectoryIdentity,
    public_parent: DirectoryIdentity,
    prepared_parent: DirectoryIdentity,
    public: String,
    prepared: String,
    source: FileRecord,
    target: FileRecord,
}
fn id(parent: &StoreDirectory) -> Result<DirectoryIdentity> {
    directory_identity_from_handle(&parent.root_handle())
}
fn validate(
    intent: &Intent,
    owner: &StoreDirectory,
    public: &StoreDirectory,
    prepared: &StoreDirectory,
    kind: OperationKind,
    max: u64,
) -> Result<()> {
    validate_leaves(kind, &intent.public, &intent.prepared)?;
    if intent.authority_binding.len() != 64
        || !intent
            .authority_binding
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(failure("invalid exchange authority binding"));
    }
    if intent.version != 1
        || intent.kind != kind
        || intent.owner != id(owner)?
        || intent.public_parent != id(public)?
        || intent.prepared_parent != id(prepared)?
        || intent.source.identity() == intent.target.identity()
        || intent.source.volume != intent.target.volume
        || intent.source.length > max
        || intent.target.length > max
    {
        return Err(failure("exchange intent authority mismatch"));
    }
    for parent in [
        &intent.owner,
        &intent.public_parent,
        &intent.prepared_parent,
    ] {
        if !matches!(parent, DirectoryIdentity::Windows { volume_serial_number, .. } if *volume_serial_number == intent.source.volume)
        {
            return Err(failure("exchange crosses volumes or platforms"));
        }
    }
    Ok(())
}
pub struct PendingExchange<'a> {
    owner: &'a StoreDirectory,
    public: &'a StoreDirectory,
    prepared: &'a StoreDirectory,
    intent: Intent,
    #[cfg(debug_assertions)]
    test_control: crate::test_control::CoreTestControl,
    source: Option<File>,
    target: File,
    state: Placement,
    max: u64,
}
impl PendingExchange<'_> {
    pub fn source(&self) -> Option<&File> {
        self.source.as_ref()
    }
    pub fn target(&self) -> &File {
        &self.target
    }
    pub fn source_record(&self) -> &FileRecord {
        &self.intent.source
    }
    pub fn target_record(&self) -> &FileRecord {
        &self.intent.target
    }
    pub fn prepared_leaf(&self) -> &str {
        &self.intent.prepared
    }
    /// Finish only after the caller validates marker transitions / rotation authority.
    /// Retires the exact old source while the journal still binds its identity.
    pub fn complete(
        mut self,
        expected: &ExchangeExpected,
        mut checkpoint: impl FnMut(ExchangeStep) -> Result<()>,
    ) -> Result<()> {
        if expected.source != self.intent.source || expected.target != self.intent.target {
            return Err(failure("exchange caller authority mismatch"));
        }
        self.check()?;
        if self.state == Placement::Initial {
            self.checkpoint(ExchangeStep::IntentPublished)?;
            checkpoint(ExchangeStep::IntentPublished)?;
            crate::platform::move_verified_regular_handle_to_raw(
                self.public,
                self.source
                    .as_ref()
                    .ok_or_else(|| failure("missing exchange source"))?,
                self.owner,
                Path::new(BACKUP),
            )?;
            self.state = Placement::Captured;
            self.check()?;
            self.checkpoint(ExchangeStep::SourceCaptured)?;
            checkpoint(ExchangeStep::SourceCaptured)?;
        }
        if self.state == Placement::Captured {
            crate::platform::move_verified_regular_handle_to_raw(
                self.prepared,
                &self.target,
                self.public,
                Path::new(&self.intent.public),
            )?;
            self.state = Placement::Published;
            self.check()?;
            self.checkpoint(ExchangeStep::TargetPublished)?;
            checkpoint(ExchangeStep::TargetPublished)?;
        }
        if self.state == Placement::Published {
            crate::platform::move_verified_regular_handle_to_raw(
                self.owner,
                self.source
                    .as_ref()
                    .ok_or_else(|| failure("missing exchange source"))?,
                self.prepared,
                Path::new(&self.intent.prepared),
            )?;
            self.state = Placement::Complete;
            self.check()?;
            self.checkpoint(ExchangeStep::ExchangeComplete)?;
            checkpoint(ExchangeStep::ExchangeComplete)?;
        }
        if self.state == Placement::Complete {
            let source = self
                .source
                .take()
                .ok_or_else(|| failure("missing retirement source"))?;
            crate::platform::retire_gc_handle(self.prepared, &source)?;
            drop(source);
            self.state = Placement::Retired;
            self.check()?;
            self.checkpoint(ExchangeStep::SourceRetired)?;
            checkpoint(ExchangeStep::SourceRetired)?;
        }
        // The journal remains authoritative until exact-source retirement completes.
        let bytes = serde_jcs::to_vec(&self.intent).map_err(|e| failure(e.to_string()))?;
        self.owner
            .quarantine_then_remove(Path::new(INTENT), &bytes, MAX_INTENT)?;
        self.checkpoint(ExchangeStep::IntentRemoved)?;
        checkpoint(ExchangeStep::IntentRemoved)?;
        Ok(())
    }
    fn checkpoint(&self, _step: ExchangeStep) -> Result<()> {
        #[cfg(debug_assertions)]
        {
            use crate::test_control::GcFault;
            let fault = match (self.intent.kind, _step) {
                (OperationKind::Marker, ExchangeStep::IntentPublished) => {
                    GcFault::AfterWindowsMarkerExchangeIntent
                }
                (OperationKind::Marker, ExchangeStep::SourceCaptured) => {
                    GcFault::AfterWindowsMarkerExchangeSourceCapture
                }
                (OperationKind::Marker, ExchangeStep::TargetPublished) => {
                    GcFault::AfterWindowsMarkerExchangeTargetPublish
                }
                (OperationKind::Marker, ExchangeStep::ExchangeComplete) => {
                    GcFault::AfterWindowsMarkerExchangeNamesComplete
                }
                (OperationKind::Marker, ExchangeStep::SourceRetired) => {
                    GcFault::AfterWindowsMarkerExchangeSourceRetire
                }
                (OperationKind::Marker, ExchangeStep::IntentRemoved) => {
                    GcFault::AfterWindowsMarkerExchangeIntentRemove
                }
                (OperationKind::Index, ExchangeStep::IntentPublished) => {
                    GcFault::AfterWindowsIndexExchangeIntent
                }
                (OperationKind::Index, ExchangeStep::SourceCaptured) => {
                    GcFault::AfterWindowsIndexExchangeSourceCapture
                }
                (OperationKind::Index, ExchangeStep::TargetPublished) => {
                    GcFault::AfterWindowsIndexExchangeTargetPublish
                }
                (OperationKind::Index, ExchangeStep::ExchangeComplete) => {
                    GcFault::AfterWindowsIndexExchangeNamesComplete
                }
                (OperationKind::Index, ExchangeStep::SourceRetired) => {
                    GcFault::AfterWindowsIndexExchangeSourceRetire
                }
                (OperationKind::Index, ExchangeStep::IntentRemoved) => {
                    GcFault::AfterWindowsIndexExchangeIntentRemove
                }
                (OperationKind::SnapshotState, ExchangeStep::IntentPublished) => {
                    GcFault::AfterWindowsSnapshotStateExchangeIntent
                }
                (OperationKind::SnapshotState, ExchangeStep::SourceCaptured) => {
                    GcFault::AfterWindowsSnapshotStateExchangeSourceCapture
                }
                (OperationKind::SnapshotState, ExchangeStep::TargetPublished) => {
                    GcFault::AfterWindowsSnapshotStateExchangeTargetPublish
                }
                (OperationKind::SnapshotState, ExchangeStep::ExchangeComplete) => {
                    GcFault::AfterWindowsSnapshotStateExchangeNamesComplete
                }
                (OperationKind::SnapshotState, ExchangeStep::SourceRetired) => {
                    GcFault::AfterWindowsSnapshotStateExchangeSourceRetire
                }
                (OperationKind::SnapshotState, ExchangeStep::IntentRemoved) => {
                    GcFault::AfterWindowsSnapshotStateExchangeIntentRemove
                }
            };
            if self.test_control.gc_fault.known() == Some(&fault) {
                return Err(KioError::new(
                    "KIO-E-GC-TEST-INTERRUPTED-001",
                    "GC exchange test fault interrupted the sweep",
                    serde_json::json!({"point": format!("{fault:?}")}),
                    ExitCode::Interrupted,
                ));
            }
        }
        Ok(())
    }
    fn check(&self) -> Result<()> {
        inspect_pending(self.owner)?;
        validate(
            &self.intent,
            self.owner,
            self.public,
            self.prepared,
            self.intent.kind,
            self.max,
        )?;
        let source_matches = match &self.source {
            Some(source) => {
                self.state != Placement::Retired && observe(source, self.max)? == self.intent.source
            }
            None => self.state == Placement::Retired,
        };
        if !source_matches || observe(&self.target, self.max)? != self.intent.target {
            return Err(failure("retained exchange files changed"));
        }
        // Restrictive pins prevent replacing either present name. Check absence too.
        let expected = match self.state {
            Placement::Initial => (true, true, false),
            Placement::Captured => (false, true, true),
            Placement::Published => (true, false, true),
            Placement::Complete => (true, true, false),
            Placement::Retired => (true, false, false),
        };
        let actual = (
            self.public.contains_entry(Path::new(&self.intent.public))?,
            self.prepared
                .contains_entry(Path::new(&self.intent.prepared))?,
            self.owner.contains_entry(Path::new(BACKUP))?,
        );
        if actual != expected {
            return Err(failure("exchange placement changed"));
        }
        Ok(())
    }
}

/// Validate any readable exchange intent before atomic journal housekeeping.
pub fn validate_pending_authority(owner: &StoreDirectory, authority_binding: &str) -> Result<()> {
    for relative in [INTENT, ".kio-atomic/write", ".kio-atomic/removed"] {
        if let Some(bytes) = owner.read_optional(Path::new(relative), MAX_INTENT)? {
            let intent: Intent =
                serde_json::from_slice(&bytes).map_err(|e| failure(e.to_string()))?;
            if serde_jcs::to_vec(&intent).map_err(|e| failure(e.to_string()))? != bytes
                || intent.authority_binding != authority_binding
            {
                return Err(failure("pending exchange authority changed"));
            }
        }
    }
    Ok(())
}

/// Recover only the owner's journal publication, then retain/validate both files.
/// This function requires the GC writer lock. It never performs exchange moves.
pub fn open_pending_under_lock<'a>(
    owner: &'a StoreDirectory,
    public: &'a StoreDirectory,
    prepared: &'a StoreDirectory,
    kind: OperationKind,
    max: u64,
    authority_binding: &str,
) -> Result<Option<PendingExchange<'a>>> {
    #[cfg(debug_assertions)]
    let test_control = crate::test_control::capture_for_operation().core;
    inspect_pending(owner)?;
    owner.recover_atomic(&[owner])?;
    let Some(bytes) = owner.read_optional(Path::new(INTENT), MAX_INTENT)? else {
        if owner.contains_entry(Path::new(BACKUP))? {
            return Err(failure("orphan exchange backup"));
        }
        return Ok(None);
    };
    let intent: Intent = serde_json::from_slice(&bytes).map_err(|e| failure(e.to_string()))?;
    if serde_jcs::to_vec(&intent).map_err(|e| failure(e.to_string()))? != bytes {
        return Err(failure("exchange intent is not canonical"));
    }
    validate(&intent, owner, public, prepared, kind, max)?;
    if intent.authority_binding != authority_binding {
        return Err(failure("exchange authority binding mismatch"));
    }
    let mut source = None;
    let mut target = None;
    let mut roles = Vec::new();
    for (parent, leaf) in [
        (public, intent.public.as_str()),
        (prepared, intent.prepared.as_str()),
        (owner, BACKUP),
    ] {
        if !parent.contains_entry(Path::new(leaf))? {
            roles.push(None);
            continue;
        }
        let file = crate::platform::open_gc_mutation(parent, Path::new(leaf), max)?;
        let observed = observe(&file, max)?;
        if observed == intent.source && source.is_none() {
            source = Some(file);
            roles.push(Some(false));
        } else if observed == intent.target && target.is_none() {
            target = Some(file);
            roles.push(Some(true));
        } else {
            return Err(failure("unknown or duplicate exchange file"));
        }
    }
    let state = placement(roles[0], roles[1], roles[2])?;
    let pending = PendingExchange {
        #[cfg(debug_assertions)]
        test_control,
        owner,
        public,
        prepared,
        intent,
        source,
        target: target.ok_or_else(|| failure("missing target"))?,
        state,
        max,
    };
    pending.check()?;
    Ok(Some(pending))
}

/// Publish the entire intent before moving either file. Both files stay pinned.
#[allow(clippy::too_many_arguments)]
pub fn begin<'a>(
    owner: &'a StoreDirectory,
    public: &'a StoreDirectory,
    public_leaf: &str,
    prepared: &'a StoreDirectory,
    prepared_leaf: &str,
    kind: OperationKind,
    expected: &ExchangeExpected,
    max: u64,
    authority_binding: &str,
    validate_initial: impl FnOnce(&File, &File) -> Result<()>,
) -> Result<PendingExchange<'a>> {
    #[cfg(debug_assertions)]
    let test_control = crate::test_control::capture_for_operation().core;
    validate_leaves(kind, public_leaf, prepared_leaf)?;
    if inspect_pending(owner)? {
        return Err(failure("exchange already pending"));
    }
    let source = crate::platform::open_gc_mutation(public, Path::new(public_leaf), max)?;
    let target = crate::platform::open_gc_mutation(prepared, Path::new(prepared_leaf), max)?;
    if observe(&source, max)? != expected.source || observe(&target, max)? != expected.target {
        return Err(failure("exchange initial authority mismatch"));
    }
    validate_initial(&source, &target)?;
    let intent = Intent {
        authority_binding: authority_binding.into(),
        version: 1,
        kind,
        owner: id(owner)?,
        public_parent: id(public)?,
        prepared_parent: id(prepared)?,
        public: public_leaf.into(),
        prepared: prepared_leaf.into(),
        source: expected.source.clone(),
        target: expected.target.clone(),
    };
    validate(&intent, owner, public, prepared, kind, max)?;
    let bytes = serde_jcs::to_vec(&intent).map_err(|e| failure(e.to_string()))?;
    if bytes.len() as u64 > MAX_INTENT {
        return Err(failure("exchange intent exceeds bound"));
    }
    source.sync_all().map_err(|e| failure(e.to_string()))?;
    target.sync_all().map_err(|e| failure(e.to_string()))?;
    owner.write_atomic(Path::new(INTENT), &bytes, Publication::CreateOnly)?;
    let pending = PendingExchange {
        #[cfg(debug_assertions)]
        test_control,
        owner,
        public,
        prepared,
        intent,
        source: Some(source),
        target,
        state: Placement::Initial,
        max,
    };
    pending.check()?;
    Ok(pending)
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::{fs, path::Path};

    #[cfg(debug_assertions)]
    #[test]
    fn atomic_journal_boot_and_cleanup_interruptions_preserve_exchange_files() {
        use crate::durability::DurabilityPoint;
        use crate::test_control::{DebugTestControl, Selector, install_scoped};
        for point in [
            DurabilityPoint::AtomicWriteStaged,
            DurabilityPoint::AtomicWritePublished,
            DurabilityPoint::AtomicRemoveReady,
            DurabilityPoint::AtomicRemoveQuarantined,
            DurabilityPoint::AtomicRemoveDeleted,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let root = StoreDirectory::open(&temp.path().canonicalize().unwrap()).unwrap();
            let mut dirs = Vec::new();
            for name in ["journal", "public", "prepared"] {
                let handle = root.create_directory(Path::new(name)).unwrap();
                dirs.push(StoreDirectory::from_retained(handle, temp.path().join(name)).unwrap());
            }
            let (owner, public, prepared) = (&dirs[0], &dirs[1], &dirs[2]);
            fs::write(public.path().join("in_progress"), b"old").unwrap();
            fs::write(prepared.path().join(".gc-retired-marker-1-2"), b"new").unwrap();
            let source =
                crate::platform::open_gc_mutation(public, Path::new("in_progress"), 64).unwrap();
            let target = crate::platform::open_gc_mutation(
                prepared,
                Path::new(".gc-retired-marker-1-2"),
                64,
            )
            .unwrap();
            let expected = ExchangeExpected {
                source: observe(&source, 64).unwrap(),
                target: observe(&target, 64).unwrap(),
            };
            drop((source, target));
            let binding = "b".repeat(64);
            // An occupied checkpoint signal causes a deterministic error at the selected boundary.
            let signal = temp.path().join("occupied-checkpoint");
            fs::write(&signal, b"occupied").unwrap();
            let mut control = DebugTestControl::default();
            control.core.durability_point = Selector::Known(point);
            control.core.durability_ready = Some(signal);
            {
                let _guard = install_scoped(control);
                let result = begin(
                    owner,
                    public,
                    "in_progress",
                    prepared,
                    ".gc-retired-marker-1-2",
                    OperationKind::Marker,
                    &expected,
                    64,
                    &binding,
                    |_, _| Ok(()),
                )
                .and_then(|pending| pending.complete(&expected, |_| Ok(())));
                assert!(result.is_err());
            }
            assert!(inspect_pending(owner).unwrap());
            let pending = open_pending_under_lock(
                owner,
                public,
                prepared,
                OperationKind::Marker,
                64,
                &binding,
            )
            .unwrap();
            if let Some(pending) = pending {
                pending.complete(&expected, |_| Ok(())).unwrap();
            }
            let (public_bytes, prepared_bytes) = if point == DurabilityPoint::AtomicWriteStaged {
                (b"old", b"new")
            } else {
                (b"new", b"old")
            };
            assert_eq!(
                fs::read(public.path().join("in_progress")).unwrap(),
                public_bytes
            );
            if point == DurabilityPoint::AtomicWriteStaged {
                assert_eq!(
                    fs::read(prepared.path().join(".gc-retired-marker-1-2")).unwrap(),
                    prepared_bytes
                );
            } else {
                assert!(
                    !prepared
                        .contains_entry(Path::new(".gc-retired-marker-1-2"))
                        .unwrap()
                );
            }
            assert!(!inspect_pending(owner).unwrap());
            assert!(
                open_pending_under_lock(
                    owner,
                    public,
                    prepared,
                    OperationKind::Marker,
                    64,
                    &binding
                )
                .unwrap()
                .is_none()
            );
        }
    }

    #[test]
    fn resume_every_namespace_interruption() {
        for interruption in [
            ExchangeStep::IntentPublished,
            ExchangeStep::SourceCaptured,
            ExchangeStep::TargetPublished,
            ExchangeStep::ExchangeComplete,
            ExchangeStep::SourceRetired,
            ExchangeStep::IntentRemoved,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let root = StoreDirectory::open(&temp.path().canonicalize().unwrap()).unwrap();
            let mut dirs = Vec::new();
            for name in ["journal", "public", "prepared"] {
                let handle = root.create_directory(Path::new(name)).unwrap();
                dirs.push(StoreDirectory::from_retained(handle, temp.path().join(name)).unwrap());
            }
            let (owner, public, prepared) = (&dirs[0], &dirs[1], &dirs[2]);
            fs::write(public.path().join("in_progress"), b"old").unwrap();
            fs::write(prepared.path().join(".gc-retired-marker-1-2"), b"new").unwrap();
            let source =
                crate::platform::open_gc_mutation(public, Path::new("in_progress"), 64).unwrap();
            let target = crate::platform::open_gc_mutation(
                prepared,
                Path::new(".gc-retired-marker-1-2"),
                64,
            )
            .unwrap();
            let expected = ExchangeExpected {
                source: observe(&source, 64).unwrap(),
                target: observe(&target, 64).unwrap(),
            };
            drop((source, target));
            let binding = "a".repeat(64);
            let pending = begin(
                owner,
                public,
                "in_progress",
                prepared,
                ".gc-retired-marker-1-2",
                OperationKind::Marker,
                &expected,
                64,
                &binding,
                |_, _| Ok(()),
            )
            .unwrap();
            if interruption == ExchangeStep::IntentPublished {
                drop(pending);
            } else {
                assert!(
                    pending
                        .complete(&expected, |step| if step == interruption {
                            Err(failure("simulated interruption"))
                        } else {
                            Ok(())
                        })
                        .is_err()
                );
            }
            let recovered = open_pending_under_lock(
                owner,
                public,
                prepared,
                OperationKind::Marker,
                64,
                &binding,
            )
            .unwrap();
            if let Some(pending) = recovered {
                assert_eq!(pending.source_record(), &expected.source);
                assert_eq!(pending.target_record(), &expected.target);
                pending.complete(&expected, |_| Ok(())).unwrap();
            } else {
                assert_eq!(interruption, ExchangeStep::IntentRemoved);
            }
            assert_eq!(fs::read(public.path().join("in_progress")).unwrap(), b"new");
            assert!(
                !prepared
                    .contains_entry(Path::new(".gc-retired-marker-1-2"))
                    .unwrap()
            );
            assert!(!inspect_pending(owner).unwrap());
        }
    }
}
