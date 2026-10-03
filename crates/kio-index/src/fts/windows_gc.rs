//! Windows GC index namespace rotation, bound to the durable GC marker.
//!
//! The core helper journals three exact-handle, no-replace moves and retains
//! the intent through exact old-source retirement. This module
//! binds that journal to the caller's index authority; completing the namespace
//! protocol does not replace the caller's SQLite generation/attestation checks.
use super::*;
use kio_core::gc::windows_exchange::{
    self as exchange, ExchangeExpected, ExchangeStep, OperationKind,
};
use kio_core::store_dir::StoreDirectory;
use std::fs::File;

const MAX_INDEX_BYTES: u64 = 4 * 1024 * 1024 * 1024;

fn core_error(error: kio_core::KioError) -> IndexError {
    IndexError::GcExchange(error)
}
fn validation_error(error: IndexError) -> kio_core::KioError {
    kio_core::KioError::new(
        "KIO-E-GC-INDEX-EXCHANGE-001",
        error.to_string(),
        serde_json::json!({}),
        kio_core::ExitCode::PermanentFailure,
    )
}
fn directory(file: &File) -> Result<StoreDirectory> {
    StoreDirectory::from_retained(
        file.try_clone()
            .map_err(|error| IndexError::Schema(format!("retain GC index directory: {error}")))?,
        PathBuf::from("<retained GC index directory>"),
    )
    .map_err(core_error)
}

fn owner(kio: &File, create: bool) -> Result<Option<StoreDirectory>> {
    let mut parent = kio
        .try_clone()
        .map_err(|error| IndexError::Schema(format!("retain GC exchange root: {error}")))?;
    for leaf in ["gc", "internal", "index-exchange"] {
        parent = match cap_fs::open_dir_nofollow(&parent, Path::new(leaf)) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !create => {
                return Ok(None);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                cap_fs::create_dir(&parent, Path::new(leaf), &cap_fs::DirOptions::new()).map_err(
                    |error| IndexError::Schema(format!("create GC exchange owner {leaf}: {error}")),
                )?;
                cap_fs::open_dir_nofollow(&parent, Path::new(leaf)).map_err(|error| {
                    IndexError::Schema(format!("open GC exchange owner {leaf}: {error}"))
                })?
            }
            Err(error) => {
                return Err(IndexError::Schema(format!(
                    "open GC exchange owner {leaf}: {error}"
                )));
            }
        };
    }
    directory(&parent).map(Some)
}

pub(super) fn ensure_no_pending(kio: &File) -> Result<()> {
    if let Some(owner) = owner(kio, false)?
        && exchange::inspect_pending(&owner).map_err(core_error)?
    {
        return Err(IndexError::Schema(
            "private index cleanup cannot run while exchange is pending".into(),
        ));
    }
    Ok(())
}

struct Authority<'a> {
    temp_leaf: &'a str,
    private_identity: &'a str,
    source_identity: &'a str,
    source_state_digest: &'a str,
    target_identity: &'a str,
}
impl Authority<'_> {
    fn binding(&self) -> Result<String> {
        validate_gc_temp_leaf(self.temp_leaf)?;
        if self.source_identity == self.target_identity
            || !is_sha256_digest(self.source_state_digest)
        {
            return Err(IndexError::Schema(
                "invalid GC index rotation authority".into(),
            ));
        }
        // A versioned canonical encoding binds every marker-owned field,
        // including the pre-move state digest which must survive rename ctime.
        let bytes = serde_jcs::to_vec(&serde_json::json!({
            "version": 1,
            "kind": "gc_index_rotation",
            "private_parent": self.private_identity,
            "temp_leaf": self.temp_leaf,
            "source_identity": self.source_identity,
            "source_state_digest": self.source_state_digest,
            "target_identity": self.target_identity,
        }))
        .map_err(|error| IndexError::Schema(format!("encode GC index authority: {error}")))?;
        Ok(kio_core::cas::lower_hex(&Sha256::digest(bytes)))
    }
    fn validate_files(&self, source: &File, target: &File, initial: bool) -> Result<()> {
        if canonical_gc_index_identity(source)? != self.source_identity
            || canonical_gc_index_identity(target)? != self.target_identity
            || (initial
                && source_file_state_digest(source_file_state(source)?) != self.source_state_digest)
        {
            return Err(IndexError::Schema(
                "GC durable index exchange inputs changed".into(),
            ));
        }
        Ok(())
    }
}

fn parents(kio: &File, authority: &Authority<'_>) -> Result<(StoreDirectory, StoreDirectory)> {
    let public = cap_fs::open_dir_nofollow(kio, Path::new("index"))
        .map_err(|error| IndexError::Schema(format!("open bound GC index parent: {error}")))?;
    let private = open_existing_gc_internal_index_dir(kio)?;
    require_gc_private_dir_identity(&private, authority.private_identity)?;
    Ok((directory(&public)?, directory(&private)?))
}

fn finish(
    pending: exchange::PendingExchange<'_>,
    authority: &Authority<'_>,
    checkpoint: impl FnMut(ExchangeStep) -> kio_core::Result<()>,
) -> Result<()> {
    if pending.prepared_leaf() != authority.temp_leaf {
        return Err(IndexError::Schema(
            "GC index exchange prepared leaf changed".into(),
        ));
    }
    let (source_volume, source_index) = pending.source_record().identity();
    if format!("windows:{source_volume:08x}:{source_index:016x}") != authority.source_identity
        || canonical_gc_index_identity(pending.target())? != authority.target_identity
    {
        return Err(IndexError::Schema(
            "GC recovered index authority changed".into(),
        ));
    }
    if let Some(source) = pending.source() {
        authority.validate_files(source, pending.target(), false)?;
    }
    // A retired source is accepted only in the helper's terminal placement,
    // under the same exact marker binding and still-verified target record.
    // The journal's stable identities/lengths/hashes were re-observed under
    // restrictive pins. The original state digest is bound by the canonical
    // authority string, not recomputed after a rename changed file timestamps.
    let expected = ExchangeExpected {
        source: pending.source_record().clone(),
        target: pending.target_record().clone(),
    };
    // The journal remains until exact old-source retirement is complete.
    // complete releases every mutation pin before app-level SQLite checks.
    pending.complete(&expected, checkpoint).map_err(core_error)
}

fn recover_authority(kio: &File, authority: &Authority<'_>) -> Result<bool> {
    let binding = authority.binding()?;
    let Some(owner) = owner(kio, false)? else {
        return Ok(false);
    };
    // inspect_pending includes an unpublished atomic intent. Never call an
    // absent public leaf an empty index while that residue requires recovery.
    if !exchange::inspect_pending(&owner).map_err(core_error)? {
        return Ok(false);
    }
    let (public, private) = parents(kio, authority)?;
    let Some(pending) = exchange::open_pending_under_lock(
        &owner,
        &public,
        &private,
        OperationKind::Index,
        MAX_INDEX_BYTES,
        &binding,
    )
    .map_err(core_error)?
    else {
        // Atomic publication can discard an unpublished intent, or complete
        // removal of an already-finished intent. No index namespace exchange
        // was performed here; the caller must inspect the actual public
        // source/target as usual. The helper still rejects orphan backups and
        // malformed journals, and an unexplained missing public index is not
        // turned into a CREATE operation.
        return Ok(false);
    };
    finish(pending, authority, |_| Ok(()))?;
    Ok(true)
}

pub(super) fn recover(
    kio: &File,
    temp_leaf: &str,
    private_identity: &str,
    source_identity: &str,
    source_state_digest: &str,
    target_identity: &str,
) -> Result<bool> {
    recover_authority(
        kio,
        &Authority {
            temp_leaf,
            private_identity,
            source_identity,
            source_state_digest,
            target_identity,
        },
    )
}

fn exchange_authority(
    kio: &File,
    authority: &Authority<'_>,
    mut checkpoint: impl FnMut(ExchangeStep) -> kio_core::Result<()>,
) -> Result<()> {
    let binding = authority.binding()?;
    if recover_authority(kio, authority)? {
        return Ok(());
    }
    let (public, private) = parents(kio, authority)?;
    // First validate the exact current source state before any new intent or
    // exchange owner is published. begin reopens and repeats the validation
    // under restrictive pins immediately before durable intent publication.
    let source = public
        .open_gc_mutation(Path::new("sqlite.db"), MAX_INDEX_BYTES)
        .map_err(core_error)?;
    let target = private
        .open_gc_mutation(Path::new(authority.temp_leaf), MAX_INDEX_BYTES)
        .map_err(core_error)?;
    authority.validate_files(&source, &target, true)?;
    let expected = ExchangeExpected {
        source: exchange::observe(&source, MAX_INDEX_BYTES).map_err(core_error)?,
        target: exchange::observe(&target, MAX_INDEX_BYTES).map_err(core_error)?,
    };
    drop(target);
    drop(source);
    let owner = owner(kio, true)?.expect("create owner returns directory");
    let pending = exchange::begin(
        &owner,
        &public,
        "sqlite.db",
        &private,
        authority.temp_leaf,
        OperationKind::Index,
        &expected,
        MAX_INDEX_BYTES,
        &binding,
        |source, target| {
            authority
                .validate_files(source, target, true)
                .map_err(validation_error)
        },
    )
    .map_err(core_error)?;
    checkpoint(ExchangeStep::IntentPublished).map_err(core_error)?;
    finish(pending, authority, checkpoint)
}

pub(super) fn exchange(
    kio: &File,
    temp_leaf: &str,
    private_identity: &str,
    source_identity: &str,
    source_state_digest: &str,
    target_identity: &str,
) -> Result<()> {
    exchange_authority(
        kio,
        &Authority {
            temp_leaf,
            private_identity,
            source_identity,
            source_state_digest,
            target_identity,
        },
        |_| Ok(()),
    )
}

pub(super) fn remove_exact(
    parent: &File,
    leaf: &str,
    expected_identity: &str,
) -> Result<PreparedGcIndexCleanup> {
    let parent = directory(parent)?;
    if !parent.contains_entry(Path::new(leaf)).map_err(core_error)? {
        return Ok(PreparedGcIndexCleanup::AlreadyAbsent);
    }
    let file = parent
        .open_gc_mutation(Path::new(leaf), MAX_INDEX_BYTES)
        .map_err(core_error)?;
    if canonical_gc_index_identity(&file)? != expected_identity {
        return Err(IndexError::Schema(
            "GC private index cleanup input changed".into(),
        ));
    }
    parent.retire_gc_handle(&file).map_err(core_error)?;
    Ok(PreparedGcIndexCleanup::Removed)
}

pub(super) fn remove_prepared(
    kio: &File,
    temp_leaf: &str,
    private_identity: &str,
    expected_identity: &str,
) -> Result<PreparedGcIndexCleanup> {
    validate_gc_temp_leaf(temp_leaf)?;
    ensure_no_pending(kio)?;
    let private = open_existing_gc_internal_index_dir(kio)?;
    require_gc_private_dir_identity(&private, private_identity)?;
    remove_exact(&private, temp_leaf, expected_identity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom, Write};

    struct Fixture {
        _directory: tempfile::TempDir,
        kio_path: PathBuf,
        kio: File,
        prepared: PreparedBoundGcIndexRotation,
        attestation: GcIndexRotationAttestation,
        config: FtsSchemaConfig,
    }
    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let kio_path = directory.path().join(".kio");
            std::fs::create_dir_all(kio_path.join("index")).unwrap();
            let config = FtsSchemaConfig {
                tokenizer: FtsTokenizer::Trigram,
            };
            let path = kio_path.join("index/sqlite.db");
            let index = SqliteFtsIndex::open(&path, config.clone()).unwrap();
            let source_generation = "01J00000000000000000000000";
            ensure_index_metadata(index.connection(), source_generation, 7).unwrap();
            drop(index);
            let kio =
                cap_fs::open_ambient_dir(&kio_path, cap_primitives::ambient_authority()).unwrap();
            let source = read_bound_gc_index_metadata(&kio, &config)
                .unwrap()
                .unwrap();
            let attestation = GcIndexRotationAttestation {
                sweep_id: "01J00000000000000000000002".into(),
                role: "pre_sweep".into(),
                plan_digest: format!("sha256:{}", "a".repeat(64)),
                source_generation: source_generation.into(),
                target_generation: "01J00000000000000000000001".into(),
            };
            let prepared = prepare_bound_gc_index_rotation(
                &kio,
                ".gc-index-windows-test",
                &attestation.target_generation,
                (&source.metadata.index_generation, &source.file_identity),
                &attestation,
                &config,
            )
            .unwrap();
            Self {
                _directory: directory,
                kio_path,
                kio,
                prepared,
                attestation,
                config,
            }
        }
        fn authority(&self) -> Authority<'_> {
            Authority {
                temp_leaf: &self.prepared.temp_leaf,
                private_identity: &self.prepared.private_dir_identity,
                source_identity: &self.prepared.source.file_identity,
                source_state_digest: &self.prepared.source_state_digest,
                target_identity: &self.prepared.target.file_identity,
            }
        }
        fn public(&self) -> PathBuf {
            self.kio_path.join("index/sqlite.db")
        }
        fn private(&self) -> PathBuf {
            self.kio_path
                .join("gc/internal/index")
                .join(&self.prepared.temp_leaf)
        }
        fn assert_target_attested(&self) {
            let (metadata, attestation, file) =
                read_bound_gc_index_rotation_attestation(&self.kio, &self.config)
                    .unwrap()
                    .unwrap();
            assert_eq!(metadata.file_identity, self.prepared.target.file_identity);
            assert_eq!(
                metadata.metadata.index_generation,
                self.attestation.target_generation
            );
            assert_eq!(attestation, self.attestation);
            drop(file);
        }
    }

    fn interruption() -> kio_core::KioError {
        kio_core::KioError::new(
            "KIO-E-GC-TEST-INTERRUPTED-001",
            "injected process interruption",
            serde_json::json!({"point": "index_exchange_checkpoint"}),
            kio_core::ExitCode::Interrupted,
        )
    }

    #[cfg(debug_assertions)]
    #[test]
    fn atomic_intent_residues_recover_at_index_caller_without_extra_retry() {
        use kio_core::durability::DurabilityPoint;
        use kio_core::test_control::{DebugTestControl, Selector, install_scoped};
        for point in [
            DurabilityPoint::AtomicWriteStaged,
            DurabilityPoint::AtomicRemoveQuarantined,
            DurabilityPoint::AtomicRemoveDeleted,
        ] {
            let fixture = Fixture::new();
            let occupied = fixture._directory.path().join("occupied-atomic-checkpoint");
            std::fs::write(&occupied, b"occupied").unwrap();
            let mut control = DebugTestControl::default();
            control.core.durability_point = Selector::Known(point);
            control.core.durability_ready = Some(occupied);
            {
                let _guard = install_scoped(control);
                assert!(
                    exchange_authority(&fixture.kio, &fixture.authority(), |_| Ok(())).is_err()
                );
            }
            let journal_owner = owner(&fixture.kio, false).unwrap().unwrap();
            assert!(exchange::inspect_pending(&journal_owner).unwrap());
            assert!(
                !recover_authority(&fixture.kio, &fixture.authority()).unwrap(),
                "atomic-only residue must not cause an extra failing gc resume: {point:?}"
            );
            assert!(!exchange::inspect_pending(&journal_owner).unwrap());
            if point == DurabilityPoint::AtomicWriteStaged {
                let source = read_bound_gc_index_metadata(&fixture.kio, &fixture.config)
                    .unwrap()
                    .unwrap();
                assert_eq!(source, fixture.prepared.source);
                assert!(fixture.private().exists());
                // The ordinary caller can begin the still-unperformed rotation
                // in this same recovery invocation, without another gc run.
                exchange_authority(&fixture.kio, &fixture.authority(), |_| Ok(())).unwrap();
            } else {
                assert!(!fixture.private().exists());
            }
            fixture.assert_target_attested();
            assert!(!recover_authority(&fixture.kio, &fixture.authority()).unwrap());
        }
    }

    #[test]
    fn every_exchange_checkpoint_recovers_and_revalidates_sqlite_attestation() {
        for stop in [
            ExchangeStep::IntentPublished,
            ExchangeStep::SourceCaptured,
            ExchangeStep::TargetPublished,
            ExchangeStep::ExchangeComplete,
            ExchangeStep::SourceRetired,
            ExchangeStep::IntentRemoved,
        ] {
            let fixture = Fixture::new();
            let error = exchange_authority(&fixture.kio, &fixture.authority(), |step| {
                if step == stop {
                    Err(interruption())
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
            let IndexError::GcExchange(error) = error else {
                panic!("exchange interruption lost its typed outcome: {error}");
            };
            assert_eq!(error.error_code(), "KIO-E-GC-TEST-INTERRUPTED-001");
            assert_eq!(error.exit_code(), kio_core::ExitCode::Interrupted);
            assert_eq!(error.context(), interruption().context());
            if stop == ExchangeStep::SourceCaptured {
                assert!(
                    !fixture.public().exists(),
                    "the journal must account for the public-name gap"
                );
            }
            let recovered = recover_authority(&fixture.kio, &fixture.authority()).unwrap();
            assert_eq!(recovered, stop != ExchangeStep::IntentRemoved);
            fixture.assert_target_attested();
            assert!(!recover_authority(&fixture.kio, &fixture.authority()).unwrap());
            assert_eq!(
                remove_prepared(
                    &fixture.kio,
                    &fixture.prepared.temp_leaf,
                    &fixture.prepared.private_dir_identity,
                    &fixture.prepared.source.file_identity
                )
                .unwrap(),
                PreparedGcIndexCleanup::AlreadyAbsent
            );
            assert!(!fixture.private().exists());
            fixture.assert_target_attested();
        }
    }

    #[test]
    fn mismatched_marker_authority_cannot_fill_public_gap() {
        let fixture = Fixture::new();
        assert!(
            exchange_authority(&fixture.kio, &fixture.authority(), |step| {
                if step == ExchangeStep::SourceCaptured {
                    Err(interruption())
                } else {
                    Ok(())
                }
            })
            .is_err()
        );
        let changed_digest = format!("sha256:{}", "b".repeat(64));
        let mut changed = fixture.authority();
        changed.source_state_digest = &changed_digest;
        assert!(recover_authority(&fixture.kio, &changed).is_err());
        assert!(!fixture.public().exists());
        assert!(fixture.private().exists());
        assert!(recover_authority(&fixture.kio, &fixture.authority()).unwrap());
        fixture.assert_target_attested();
    }

    #[test]
    fn source_state_change_fails_before_intent_or_namespace_mutation() {
        let fixture = Fixture::new();
        let mut writer = std::fs::OpenOptions::new()
            .append(true)
            .open(fixture.public())
            .unwrap();
        writer.write_all(b"changed after preparation").unwrap();
        writer.sync_all().unwrap();
        drop(writer);
        let before = std::fs::read(fixture.public()).unwrap();
        assert!(exchange_authority(&fixture.kio, &fixture.authority(), |_| Ok(())).is_err());
        assert_eq!(std::fs::read(fixture.public()).unwrap(), before);
        assert!(fixture.private().exists());
        assert!(owner(&fixture.kio, false).unwrap().is_none());
    }

    #[test]
    fn restored_mtime_does_not_hide_source_mutation() {
        let fixture = Fixture::new();
        let before = std::fs::metadata(fixture.public()).unwrap();
        let mut writer = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(fixture.public())
            .unwrap();
        writer.seek(SeekFrom::End(-1)).unwrap();
        let mut byte = [0_u8; 1];
        writer.read_exact(&mut byte).unwrap();
        writer.seek(SeekFrom::End(-1)).unwrap();
        writer.write_all(&[byte[0] ^ 1]).unwrap();
        writer.sync_all().unwrap();
        writer.set_modified(before.modified().unwrap()).unwrap();
        drop(writer);
        let after = std::fs::metadata(fixture.public()).unwrap();
        assert_eq!(after.len(), before.len());
        assert_eq!(after.modified().unwrap(), before.modified().unwrap());
        assert!(exchange_authority(&fixture.kio, &fixture.authority(), |_| Ok(())).is_err());
        assert!(owner(&fixture.kio, false).unwrap().is_none());
        assert!(fixture.public().exists());
        assert!(fixture.private().exists());
    }

    #[test]
    fn replaced_target_and_private_parent_fail_closed() {
        for replace_parent in [false, true] {
            let fixture = Fixture::new();
            assert!(
                exchange_authority(&fixture.kio, &fixture.authority(), |step| {
                    if step == ExchangeStep::SourceCaptured {
                        Err(interruption())
                    } else {
                        Ok(())
                    }
                })
                .is_err()
            );
            if replace_parent {
                let parent = fixture.private().parent().unwrap().to_path_buf();
                std::fs::rename(&parent, parent.with_file_name("saved-index")).unwrap();
                std::fs::create_dir(&parent).unwrap();
            } else {
                let bytes = std::fs::read(fixture.private()).unwrap();
                std::fs::rename(fixture.private(), fixture.private().with_extension("saved"))
                    .unwrap();
                std::fs::write(fixture.private(), &bytes).unwrap();
            }
            assert!(recover_authority(&fixture.kio, &fixture.authority()).is_err());
            assert!(!fixture.public().exists());
        }
    }

    #[test]
    fn cleanup_refuses_pending_exchange_and_reclaims_exact_retained_source() {
        let fixture = Fixture::new();
        let mut old_reader = File::open(fixture.public()).unwrap();
        assert!(
            exchange_authority(&fixture.kio, &fixture.authority(), |step| {
                if step == ExchangeStep::SourceCaptured {
                    Err(interruption())
                } else {
                    Ok(())
                }
            })
            .is_err()
        );
        assert!(
            remove_prepared(
                &fixture.kio,
                &fixture.prepared.temp_leaf,
                &fixture.prepared.private_dir_identity,
                &fixture.prepared.target.file_identity
            )
            .is_err()
        );
        assert!(recover_authority(&fixture.kio, &fixture.authority()).unwrap());
        fixture.assert_target_attested();
        assert_eq!(
            remove_prepared(
                &fixture.kio,
                &fixture.prepared.temp_leaf,
                &fixture.prepared.private_dir_identity,
                &fixture.prepared.source.file_identity
            )
            .unwrap(),
            PreparedGcIndexCleanup::AlreadyAbsent
        );
        use cap_fs::_WindowsByHandle;
        assert_eq!(
            cap_fs::Metadata::from_file(&old_reader)
                .unwrap()
                .number_of_links(),
            Some(0)
        );
        assert_eq!(old_reader.metadata().unwrap().len(), 0);
        old_reader.seek(SeekFrom::Start(0)).unwrap();
        let mut remaining = Vec::new();
        old_reader.read_to_end(&mut remaining).unwrap();
        assert!(remaining.is_empty());
        assert_eq!(
            remove_prepared(
                &fixture.kio,
                &fixture.prepared.temp_leaf,
                &fixture.prepared.private_dir_identity,
                &fixture.prepared.source.file_identity
            )
            .unwrap(),
            PreparedGcIndexCleanup::AlreadyAbsent
        );
    }

    #[test]
    fn cleanup_identity_substitution_and_hardlink_are_preserved() {
        let fixture = Fixture::new();
        assert!(
            remove_prepared(
                &fixture.kio,
                &fixture.prepared.temp_leaf,
                &fixture.prepared.private_dir_identity,
                &fixture.prepared.source.file_identity
            )
            .is_err()
        );
        let bytes = std::fs::read(fixture.private()).unwrap();
        std::fs::hard_link(fixture.private(), fixture.private().with_extension("alias")).unwrap();
        assert!(
            remove_prepared(
                &fixture.kio,
                &fixture.prepared.temp_leaf,
                &fixture.prepared.private_dir_identity,
                &fixture.prepared.target.file_identity
            )
            .is_err()
        );
        assert_eq!(std::fs::read(fixture.private()).unwrap(), bytes);
    }
}
