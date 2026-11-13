//! Create-only authority-bound ledger backups, held in private disk stages.
use crate::platform::ledger_snapshot::PrivateStorage;
use crate::{PipelineError, Result};
use kio_core::control::ControlHandle;
use kio_core::store_dir::{
    ATOMIC_WORKSPACE_DIR, AtomicWorkspaceState, Publication, StageWriter, StoreDirectory,
};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{io::Read, path::Path, sync::Arc};
const MAX_RECORD: usize = 16 * 1024;
const MAX_MANIFEST: u64 = 16 * 1024;
const IMAGE: &str = "ledger.sqlite";

#[derive(Clone, Debug)]
pub struct LedgerBackup {
    pub(crate) ledger_id: String,
    pub(crate) era: String,
    pub(crate) sequence: u64,
    image: Arc<BackupImage>,
    pub(crate) authority: Vec<u8>,
    pub(crate) checkpoint: Vec<u8>,
}
struct BackupImage {
    store: StoreDirectory,
    _storage: PrivateStorage,
}
impl Drop for BackupImage {
    fn drop(&mut self) {
        // PrivateStorage's Windows cleanup accepts only SQLite leaves. Retire
        // our recorded stage and clean atomic workspace before releasing it;
        // never infer ownership of an unjournaled or mismatched stage.
        let cleanup = || -> Result<()> {
            if let Some(bytes) = self
                .store
                .read_optional(Path::new("ledger.sqlite.json"), MAX_MANIFEST)
                .map_err(store_err)?
            {
                let identity = serde_json::from_slice(&bytes)
                    .map_err(|e| err("KIO-E-LEDGER-BACKUP-MANIFEST-001", e.to_string()))?;
                self.store
                    .discard_stage(Path::new(IMAGE), &identity)
                    .map_err(store_err)?;
                self.store
                    .remove_file(Path::new("ledger.sqlite.json"))
                    .map_err(store_err)?;
            }
            if self.store.inspect_atomic().map_err(store_err)? == AtomicWorkspaceState::Clean {
                self.store
                    .remove_directory_all(Path::new(ATOMIC_WORKSPACE_DIR))
                    .map_err(store_err)?;
            }
            Ok(())
        };
        let _ = cleanup();
    }
}
impl std::fmt::Debug for BackupImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BackupImage(private disk stage)")
    }
}
impl BackupImage {
    fn create() -> Result<Self> {
        let storage = PrivateStorage::create(
            #[cfg(test)]
            None,
        )
        .map_err(snapshot_error)?;
        let store = storage.directory().map_err(snapshot_error)?;
        Ok(Self {
            store,
            _storage: storage,
        })
    }
    fn reader(&self) -> Result<std::fs::File> {
        self.store
            .open_regular_read(Path::new(IMAGE), u64::MAX)
            .map_err(store_err)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestoreReadiness {
    Ready,
    HistoryEvidenceRequired,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u8,
    ledger_id: String,
    era: String,
    sequence: u64,
    sqlite: DigestRecord,
    authority: DigestRecord,
    checkpoint: DigestRecord,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct DigestRecord {
    bytes: u64,
    sha256: String,
}
impl LedgerBackup {
    pub(crate) fn new(
        ledger_id: String,
        era: String,
        sequence: u64,
        source: &Connection,
        authority: Vec<u8>,
        checkpoint: Vec<u8>,
    ) -> Result<Self> {
        let image = BackupImage::create()?;
        let stage = new_stage(&image.store, IMAGE)?;
        let identity = stage.identity().clone();
        stage.sync().map_err(store_err)?;
        // SQLite's Windows VFS does not share DELETE access. Close the R1
        // writer before SQLite opens this private file, then verify its saved
        // physical identity and nonce once SQLite has closed it again.
        drop(stage);
        {
            let mut target = Connection::open_with_flags(
                image.store.path().join(IMAGE),
                OpenFlags::SQLITE_OPEN_READ_WRITE,
            )
            .map_err(sqlite_error)?;
            let copied = (|| {
                let copy = rusqlite::backup::Backup::new(source, &mut target)?;
                loop {
                    match copy.step(128)? {
                        rusqlite::backup::StepResult::Done => return Ok(()),
                        rusqlite::backup::StepResult::More => {}
                        _ => {
                            return Err(rusqlite::Error::SqliteFailure(
                                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                                None,
                            ));
                        }
                    }
                }
            })();
            copied.map_err(|e| sqlite_error_at(e, Some(&target)))?;
            target
                .pragma_update(None, "journal_mode", "DELETE")
                .map_err(|e| sqlite_error_at(e, Some(&target)))?;
            target.close().map_err(|(_, error)| sqlite_error(error))?;
        }
        identity.verify(&image.reader()?).map_err(store_err)?;
        let backup = Self {
            ledger_id,
            era,
            sequence,
            image: Arc::new(image),
            authority,
            checkpoint,
        };
        backup.validate()?;
        Ok(backup)
    }
    #[cfg(test)]
    pub(crate) fn corrupt_image_for_test(&self) {
        use std::io::{Seek, SeekFrom, Write};
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.image.store.path().join(IMAGE))
            .unwrap();
        file.seek(SeekFrom::End(-1)).unwrap();
        let mut b = [0];
        file.read_exact(&mut b).unwrap();
        b[0] ^= 1;
        file.seek(SeekFrom::End(-1)).unwrap();
        file.write_all(&b).unwrap();
    }
    pub(crate) fn identity(&self) -> (&str, &str, u64) {
        (&self.ledger_id, &self.era, self.sequence)
    }
    pub(crate) fn sqlite_digest(&self) -> Result<String> {
        Ok(digest_reader(self.image.reader()?, None)?.sha256)
    }
    pub(crate) fn validate(&self) -> Result<()> {
        let conn = Connection::open_with_flags(
            self.image.store.path().join(IMAGE),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(sqlite_error)?;
        super::lifecycle::validate_backup_image(
            &conn,
            self.identity(),
            &self.authority,
            &self.checkpoint,
        )?;
        let mut check = conn
            .prepare("PRAGMA integrity_check")
            .map_err(sqlite_error)?;
        let mut rows = check.query([]).map_err(sqlite_error)?;
        let mut count = 0;
        while let Some(row) = rows.next().map_err(sqlite_error)? {
            let result: String = row.get(0).map_err(sqlite_error)?;
            if result != "ok" {
                return Err(err("KIO-E-LEDGER-BACKUP-INTEGRITY-001", result));
            }
            count += 1;
        }
        if count != 1 {
            return Err(err(
                "KIO-E-LEDGER-BACKUP-INTEGRITY-001",
                "missing integrity result",
            ));
        }
        Ok(())
    }
    /// Stream into a retained private stage, verify the exact bytes written,
    /// and publish only after the complete image is present.
    pub(crate) fn copy_database(&self, target: &StoreDirectory, leaf: &Path) -> Result<()> {
        self.copy_database_with(target, leaf, |input, stage| {
            digest_reader(input, Some(stage))
        })
    }
    fn copy_database_with(
        &self,
        target: &StoreDirectory,
        leaf: &Path,
        copy: impl FnOnce(std::fs::File, &mut StageWriter) -> Result<DigestRecord>,
    ) -> Result<()> {
        let stage_name = format!("{}.backup-stage", leaf.to_string_lossy());
        self.retire_copy_stage(target, leaf)?;
        let mut stage = new_stage(target, &stage_name)?;
        let actual = copy(self.image.reader()?, &mut stage)?;
        stage.sync().map_err(store_err)?;
        let written = digest_reader(
            target
                .open_regular_read(Path::new(&stage_name), u64::MAX)
                .map_err(store_err)?,
            None,
        )?;
        if written != actual || actual.sha256 != self.sqlite_digest()? {
            return Err(err(
                "KIO-E-LEDGER-BACKUP-MANIFEST-001",
                "image changed during copy",
            ));
        }
        stage
            .publish(target, leaf, Publication::CreateOnly)
            .map_err(store_err)?;
        target
            .remove_file(Path::new(&format!("{stage_name}.json")))
            .map_err(store_err)?;
        target.sync().map_err(store_err)
    }
    pub(crate) fn retire_copy_stage(&self, target: &StoreDirectory, leaf: &Path) -> Result<()> {
        let name = format!("{}.backup-stage", leaf.to_string_lossy());
        let journal = format!("{name}.json");
        if let Some(bytes) = target
            .read_optional(Path::new(&journal), MAX_MANIFEST)
            .map_err(store_err)?
        {
            target
                .ensure_owner_private(Path::new(&journal))
                .map_err(store_err)?;
            let identity = serde_json::from_slice(&bytes)
                .map_err(|e| err("KIO-E-LEDGER-BACKUP-MANIFEST-001", e.to_string()))?;
            target
                .discard_stage(Path::new(&name), &identity)
                .map_err(store_err)?;
            target.remove_file(Path::new(&journal)).map_err(store_err)?;
            target.sync().map_err(store_err)?;
        }
        Ok(())
    }
    pub fn publish_create_only(&self, destination: &Path) -> Result<()> {
        self.publish_create_only_with_control(destination, &ControlHandle::default())
    }
    pub fn publish_create_only_with_control(
        &self,
        destination: &Path,
        control: &ControlHandle,
    ) -> Result<()> {
        self.validate()?;
        let s = bind_private_backup_destination(destination, control)?;
        if !s.entries(Path::new("")).map_err(store_err)?.is_empty() {
            return Err(err(
                "KIO-E-LEDGER-BACKUP-EXISTS-001",
                "backup destination is not empty",
            ));
        }
        let m = Manifest {
            version: 1,
            ledger_id: self.ledger_id.clone(),
            era: self.era.clone(),
            sequence: self.sequence,
            sqlite: digest_reader(self.image.reader()?, None)?,
            authority: digest(&self.authority),
            checkpoint: digest(&self.checkpoint),
        };
        self.copy_database(&s, Path::new("ledger.sqlite"))?;
        put(&s, "authority.json", &self.authority)?;
        put(&s, "checkpoint.json", &self.checkpoint)?;
        let bytes = serde_json::to_vec(&m)
            .map_err(|e| err("KIO-E-LEDGER-BACKUP-MANIFEST-001", e.to_string()))?;
        put(&s, "manifest.json", &bytes)?;
        s.sync().map_err(store_err)
    }
    pub fn load(destination: &Path) -> Result<Self> {
        Self::load_with_control(destination, ControlHandle::default())
    }
    pub fn load_with_control(destination: &Path, control: ControlHandle) -> Result<Self> {
        let s = bind_private_backup_destination(destination, &control)?;
        if s.inspect_atomic().map_err(store_err)? == AtomicWorkspaceState::Pending {
            return Err(err(
                "KIO-E-LEDGER-BACKUP-MANIFEST-001",
                "backup atomic workspace is pending",
            ));
        }
        let entries = s.entries(Path::new("")).map_err(store_err)?;
        if entries
            .iter()
            .filter(|e| e.name != ATOMIC_WORKSPACE_DIR)
            .count()
            != 4
        {
            return Err(err(
                "KIO-E-LEDGER-BACKUP-MANIFEST-001",
                "backup artifact has missing or unexpected entries",
            ));
        }
        let read = |name: &str, max| -> Result<Vec<u8>> {
            s.ensure_owner_private(Path::new(name)).map_err(store_err)?;
            s.read_optional(Path::new(name), max)
                .map_err(store_err)?
                .ok_or_else(|| {
                    err(
                        "KIO-E-LEDGER-BACKUP-MANIFEST-001",
                        "backup artifact is incomplete",
                    )
                })
        };
        let m: Manifest = serde_json::from_slice(&read("manifest.json", MAX_MANIFEST)?)
            .map_err(|e| err("KIO-E-LEDGER-BACKUP-MANIFEST-001", e.to_string()))?;
        if m.version != 1
            || m.authority.bytes > MAX_RECORD as u64
            || m.checkpoint.bytes > MAX_RECORD as u64
        {
            return Err(err(
                "KIO-E-LEDGER-BACKUP-MANIFEST-001",
                "invalid backup manifest",
            ));
        }
        let authority = read("authority.json", m.authority.bytes)?;
        let checkpoint = read("checkpoint.json", m.checkpoint.bytes)?;
        s.ensure_owner_private(Path::new("ledger.sqlite"))
            .map_err(store_err)?;
        let input = s
            .open_regular_read(Path::new("ledger.sqlite"), m.sqlite.bytes)
            .map_err(store_err)?;
        let image = BackupImage::create()?;
        let mut stage = new_stage(&image.store, IMAGE)?;
        let actual = digest_reader(input, Some(&mut stage))?;
        if actual != m.sqlite
            || digest(&authority) != m.authority
            || digest(&checkpoint) != m.checkpoint
        {
            return Err(err(
                "KIO-E-LEDGER-BACKUP-MANIFEST-001",
                "backup digest mismatch",
            ));
        }
        stage.sync().map_err(store_err)?;
        drop(stage);
        let backup = Self {
            ledger_id: m.ledger_id,
            era: m.era,
            sequence: m.sequence,
            image: Arc::new(image),
            authority,
            checkpoint,
        };
        backup.validate()?;
        Ok(backup)
    }
}
fn new_stage(store: &StoreDirectory, name: &str) -> Result<StageWriter> {
    let stage = StageWriter::create(store, Path::new(name)).map_err(store_err)?;
    let journal = serde_json::to_vec(stage.identity())
        .map_err(|e| err("KIO-E-LEDGER-BACKUP-MANIFEST-001", e.to_string()))?;
    put(store, &format!("{name}.json"), &journal)?;
    Ok(stage)
}
fn put(s: &StoreDirectory, n: &str, b: &[u8]) -> Result<()> {
    s.write_atomic(Path::new(n), b, Publication::CreateOnly)
        .map_err(store_err)?;
    s.ensure_owner_private(Path::new(n)).map_err(store_err)
}
fn bind_private_backup_destination(
    destination: &Path,
    control: &ControlHandle,
) -> Result<StoreDirectory> {
    let parent = destination.parent().ok_or_else(|| {
        err(
            "KIO-E-LEDGER-BACKUP-PRIVATE-001",
            "backup destination has no external parent",
        )
    })?;
    let leaf = destination.file_name().ok_or_else(|| {
        err(
            "KIO-E-LEDGER-BACKUP-PRIVATE-001",
            "backup destination has no private leaf",
        )
    })?;
    let external = kio_core::private_fs::retain_external_directory_with_control(parent, control)
        .map_err(store_err)?;
    kio_core::private_fs::verify_private_directory_at(&external, Path::new(leaf)).map_err(store_err)
}
fn digest(b: &[u8]) -> DigestRecord {
    DigestRecord {
        bytes: b.len() as u64,
        sha256: Sha256::digest(b)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    }
}
fn digest_reader(
    mut input: impl Read,
    mut output: Option<&mut StageWriter>,
) -> Result<DigestRecord> {
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    let mut buf = [0; 65536];
    loop {
        let n = input.read(&mut buf).map_err(|e| {
            snapshot_error(super::snapshot::snapshot_io_error("read backup image", e))
        })?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
        bytes = bytes
            .checked_add(n as u64)
            .ok_or_else(|| err("KIO-E-LEDGER-BACKUP-SIZE-001", "image size overflow"))?;
        if let Some(stage) = output.as_deref_mut() {
            stage.write_chunk(&buf[..n]).map_err(store_err)?;
        }
    }
    Ok(DigestRecord {
        bytes,
        sha256: hash.finalize().iter().map(|b| format!("{b:02x}")).collect(),
    })
}
fn err(c: &'static str, m: impl Into<String>) -> PipelineError {
    PipelineError::contract(c, m)
}
fn store_err(e: kio_core::KioError) -> PipelineError {
    if let Some(limit) = e.resource_limit() {
        PipelineError::ResourceLimit(limit.clone())
    } else {
        err("KIO-E-LEDGER-BACKUP-PRIVATE-001", e.to_string())
    }
}
pub(crate) fn snapshot_error(e: super::snapshot::AttemptError) -> PipelineError {
    match e {
        super::snapshot::AttemptError::ResourceLimit(mut limit) => {
            limit.stage = "ledger_backup".into();
            PipelineError::ResourceLimit(limit)
        }
        other => err("KIO-E-LEDGER-BACKUP-INTEGRITY-001", format!("{other:?}")),
    }
}
fn sqlite_error(e: rusqlite::Error) -> PipelineError {
    sqlite_error_at(e, None)
}
fn sqlite_error_at(e: rusqlite::Error, conn: Option<&Connection>) -> PipelineError {
    let os = super::snapshot::sqlite_system_error(conn);
    match super::snapshot::sqlite_resource(&e, os.as_ref()) {
        Some(mut limit) => {
            limit.stage = "ledger_backup".into();
            PipelineError::ResourceLimit(limit)
        }
        None => e.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn backup() -> LedgerBackup {
        let temp = tempfile::tempdir().unwrap();
        super::super::LedgerDb::initialize(
            temp.path()
                .canonicalize()
                .unwrap()
                .join("device/ledger.sqlite"),
        )
        .unwrap()
        .backup()
        .unwrap()
    }

    struct PrivateTestDirectory {
        _temporary: tempfile::TempDir,
        path: std::path::PathBuf,
    }

    impl PrivateTestDirectory {
        fn path(&self) -> &Path {
            &self.path
        }
    }

    fn private_tempdir() -> PrivateTestDirectory {
        let temporary = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap())
            .expect("backup fixture container");
        let retained = StoreDirectory::open(temporary.path()).expect("fixture container");
        let _private = retained
            .create_directory(Path::new("private"))
            .expect("create private backup directory");
        PrivateTestDirectory {
            path: temporary.path().join("private"),
            _temporary: temporary,
        }
    }

    #[cfg(unix)]
    #[test]
    fn backup_uses_external_alias_but_refuses_linked_private_destination() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let real = temporary.path().join("external");
        fs::create_dir(&real).unwrap();
        let alias = temporary.path().join("alias");
        symlink(&real, &alias).unwrap();
        let external = kio_core::private_fs::retain_external_directory(&real).unwrap();
        external.create_directory(Path::new("private")).unwrap();

        let destination = alias.join("private");
        let expected = backup();
        expected.publish_create_only(&destination).unwrap();
        let reopened = LedgerBackup::load(&destination).unwrap();
        assert_eq!(reopened.identity(), expected.identity());

        symlink(real.join("private"), real.join("linked-private")).unwrap();
        assert!(LedgerBackup::load(&alias.join("linked-private")).is_err());
    }

    #[cfg(debug_assertions)]
    #[test]
    fn owner_control_backup_publication_isolation() {
        use kio_core::control::ControlHandle;
        use kio_core::durability::DurabilityPoint;
        use kio_core::test_control::{CoreTestControl, DebugTestControl, Selector};
        let selected = private_tempdir();
        let sibling = private_tempdir();
        let occupied = selected
            .path()
            .parent()
            .unwrap()
            .join("occupied-backup-barrier");
        fs::write(&occupied, b"occupied").unwrap();
        let control = ControlHandle::new(DebugTestControl {
            core: CoreTestControl {
                durability_point: Selector::Known(DurabilityPoint::AtomicWriteStaged),
                durability_ready: Some(occupied.clone()),
                ..Default::default()
            },
            ..Default::default()
        });
        let backup = backup();
        let error = backup
            .publish_create_only_with_control(selected.path(), &control)
            .expect_err("selected backup publication must reach its captured durability barrier");
        assert!(
            error
                .to_string()
                .contains("KIO-E-LEDGER-BACKUP-PRIVATE-001"),
            "{error}"
        );
        assert_eq!(fs::read(&occupied).unwrap(), b"occupied");
        assert!(!selected.path().join("ledger.sqlite").exists());
        backup
            .publish_create_only_with_control(sibling.path(), &ControlHandle::default())
            .unwrap();
        let loaded =
            LedgerBackup::load_with_control(sibling.path(), ControlHandle::default()).unwrap();
        assert_eq!(loaded.identity(), backup.identity());
        assert_eq!(
            loaded.sqlite_digest().unwrap(),
            backup.sqlite_digest().unwrap()
        );
        assert_eq!(
            LedgerBackup::load(sibling.path()).unwrap().identity(),
            backup.identity()
        );
    }

    #[test]
    fn load_accepts_the_clean_atomic_workspace_created_by_publication() {
        let directory = private_tempdir();
        let backup = backup();
        backup
            .publish_create_only(directory.path())
            .expect("create-only publication");
        assert_eq!(
            LedgerBackup::load(directory.path())
                .expect("clean atomic workspace must not hide payloads")
                .identity(),
            backup.identity()
        );
    }

    #[test]
    fn load_accepts_a_complete_four_payload_backup_without_workspace() {
        let published = private_tempdir();
        let copied = private_tempdir();
        let backup = backup();
        backup.publish_create_only(published.path()).unwrap();
        backup.publish_create_only(copied.path()).unwrap();
        let copied_store = kio_core::private_fs::verify_private_directory(copied.path()).unwrap();
        copied_store
            .remove_directory_all(Path::new(ATOMIC_WORKSPACE_DIR))
            .unwrap();
        let loaded = LedgerBackup::load(copied.path()).expect("four payload backup");
        assert_eq!(loaded.identity(), backup.identity());
        assert_eq!(
            loaded.sqlite_digest().unwrap(),
            backup.sqlite_digest().unwrap()
        );
        assert_eq!(
            fs::read(published.path().join("ledger.sqlite")).unwrap(),
            fs::read(copied.path().join("ledger.sqlite")).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn pending_workspace_refuses_without_modifying_backup_payloads() {
        use std::os::unix::fs::PermissionsExt;

        let directory = private_tempdir();
        let backup = backup();
        backup.publish_create_only(directory.path()).unwrap();
        let pending = directory.path().join(ATOMIC_WORKSPACE_DIR).join("write");
        fs::write(&pending, b"residue").unwrap();
        fs::set_permissions(&pending, fs::Permissions::from_mode(0o600)).unwrap();
        let before = fs::read(directory.path().join("ledger.sqlite")).unwrap();
        assert!(LedgerBackup::load(directory.path()).is_err());
        assert_eq!(
            fs::read(directory.path().join("ledger.sqlite")).unwrap(),
            before
        );
        assert!(pending.exists());
    }
    #[test]
    fn private_image_releases_its_storage_on_drop() {
        let backup = backup();
        let private = backup.image.store.path().to_owned();
        assert!(private.exists());
        drop(backup);
        assert!(!private.exists());
    }

    #[test]
    fn corrupt_image_is_rejected_before_any_backup_payload_is_published() {
        let backup = backup();
        backup.corrupt_image_for_test();
        let directory = private_tempdir();
        assert!(backup.publish_create_only(directory.path()).is_err());
        assert!(!directory.path().join("manifest.json").exists());
        assert!(!directory.path().join("ledger.sqlite").exists());
    }

    #[test]
    fn load_rejects_corruption_even_when_manifest_hash_is_recomputed() {
        use std::io::Write;
        let backup = backup();
        let directory = private_tempdir();
        backup.publish_create_only(directory.path()).unwrap();
        let payload = directory.path().join("ledger.sqlite");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&payload)
            .unwrap()
            .write_all(b"broken database!")
            .unwrap();
        let manifest = directory.path().join("manifest.json");
        let mut m: Manifest = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
        m.sqlite = digest_reader(fs::File::open(&payload).unwrap(), None).unwrap();
        fs::write(manifest, serde_json::to_vec(&m).unwrap()).unwrap();
        assert!(LedgerBackup::load(directory.path()).is_err());
    }

    #[test]
    fn incomplete_or_exhausted_copy_is_not_published_and_its_exact_stage_can_retry() {
        let backup = backup();
        for exhausted in [false, true] {
            let directory = private_tempdir();
            let store = StoreDirectory::open(directory.path()).unwrap();
            let error = backup
                .copy_database_with(&store, Path::new("ledger.sqlite"), |_, stage| {
                    stage.write_chunk(b"partial image").map_err(store_err)?;
                    if exhausted {
                        return Err(snapshot_error(super::super::snapshot::snapshot_io_error(
                            "write backup",
                            std::io::Error::from(std::io::ErrorKind::StorageFull),
                        )));
                    }
                    Err(err("TEST-INTERRUPTED", "copy interrupted"))
                })
                .unwrap_err();
            if exhausted {
                assert!(matches!(
                    error,
                    PipelineError::ResourceLimit(kio_core::error::ResourceLimit {
                        resource: kio_core::error::Resource::Storage,
                        ..
                    })
                ));
            }
            assert!(!directory.path().join("ledger.sqlite").exists());
            assert!(!directory.path().join("manifest.json").exists());
            assert!(LedgerBackup::load(directory.path()).is_err());
            assert!(directory.path().join("ledger.sqlite.backup-stage").exists());
            backup
                .copy_database(&store, Path::new("ledger.sqlite"))
                .unwrap();
            assert!(!directory.path().join("ledger.sqlite.backup-stage").exists());
            assert!(
                !directory
                    .path()
                    .join("ledger.sqlite.backup-stage.json")
                    .exists()
            );
            let copied = digest_reader(
                fs::File::open(directory.path().join("ledger.sqlite")).unwrap(),
                None,
            )
            .unwrap();
            assert_eq!(copied.sha256, backup.sqlite_digest().unwrap());
        }
    }
}
