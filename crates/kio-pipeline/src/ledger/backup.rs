//! Create-only authority-bound ledger backups.
use crate::{PipelineError, Result};
use kio_core::store_dir::{
    ATOMIC_WORKSPACE_DIR, AtomicWorkspaceState, Publication, StoreDirectory,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
const MAX_SQLITE: usize = 512 * 1024 * 1024;
const MAX_RECORD: usize = 16 * 1024;
const MAX_MANIFEST: u64 = 16 * 1024;

#[derive(Clone, Debug)]
pub struct LedgerBackup {
    pub(crate) ledger_id: String,
    pub(crate) era: String,
    pub(crate) sequence: u64,
    pub(crate) sqlite: Vec<u8>,
    pub(crate) authority: Vec<u8>,
    pub(crate) checkpoint: Vec<u8>,
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
        sqlite: Vec<u8>,
        authority: Vec<u8>,
        checkpoint: Vec<u8>,
    ) -> Result<Self> {
        if sqlite.len() > MAX_SQLITE
            || authority.len() > MAX_RECORD
            || checkpoint.len() > MAX_RECORD
        {
            return Err(err(
                "KIO-E-LEDGER-BACKUP-SIZE-001",
                "backup source exceeds size limit",
            ));
        }
        Ok(Self {
            ledger_id,
            era,
            sequence,
            sqlite,
            authority,
            checkpoint,
        })
    }
    pub(crate) fn identity(&self) -> (&str, &str, u64) {
        (&self.ledger_id, &self.era, self.sequence)
    }
    pub(crate) fn sqlite_digest(&self) -> String {
        digest(&self.sqlite).sha256
    }
    pub fn publish_create_only(&self, destination: &Path) -> Result<()> {
        let s = kio_core::private_fs::verify_private_directory(destination).map_err(store_err)?;
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
            sqlite: digest(&self.sqlite),
            authority: digest(&self.authority),
            checkpoint: digest(&self.checkpoint),
        };
        put(&s, "ledger.sqlite", &self.sqlite)?;
        put(&s, "authority.json", &self.authority)?;
        put(&s, "checkpoint.json", &self.checkpoint)?;
        let bytes = serde_json::to_vec(&m)
            .map_err(|e| err("KIO-E-LEDGER-BACKUP-MANIFEST-001", e.to_string()))?;
        put(&s, "manifest.json", &bytes)?;
        s.sync().map_err(store_err)
    }
    pub fn load(destination: &Path) -> Result<Self> {
        let s = kio_core::private_fs::verify_private_directory(destination).map_err(store_err)?;
        let atomic = s.inspect_atomic().map_err(|_| {
            err(
                "KIO-E-LEDGER-BACKUP-MANIFEST-001",
                "backup atomic workspace is invalid",
            )
        })?;
        if atomic == AtomicWorkspaceState::Pending {
            return Err(err(
                "KIO-E-LEDGER-BACKUP-MANIFEST-001",
                "backup atomic workspace is pending",
            ));
        }
        let entries = s.entries(Path::new("")).map_err(store_err)?;
        if entries
            .iter()
            .filter(|entry| entry.name != ATOMIC_WORKSPACE_DIR)
            .count()
            != 4
        {
            return Err(err(
                "KIO-E-LEDGER-BACKUP-MANIFEST-001",
                "backup artifact has missing or unexpected entries",
            ));
        }
        let read = |n: &str, max: u64| -> Result<Vec<u8>> {
            s.ensure_owner_private(Path::new(n)).map_err(store_err)?;
            s.read_optional(Path::new(n), max)
                .map_err(store_err)?
                .ok_or_else(|| {
                    err(
                        "KIO-E-LEDGER-BACKUP-MANIFEST-001",
                        "backup artifact is incomplete",
                    )
                })
        };
        let bytes = read("manifest.json", MAX_MANIFEST)?;
        let m: Manifest = serde_json::from_slice(&bytes)
            .map_err(|e| err("KIO-E-LEDGER-BACKUP-MANIFEST-001", e.to_string()))?;
        if m.version != 1
            || m.sqlite.bytes > MAX_SQLITE as u64
            || m.authority.bytes > MAX_RECORD as u64
            || m.checkpoint.bytes > MAX_RECORD as u64
        {
            return Err(err(
                "KIO-E-LEDGER-BACKUP-MANIFEST-001",
                "manifest declares an oversized backup file",
            ));
        }
        let sqlite = read("ledger.sqlite", m.sqlite.bytes)?;
        let authority = read("authority.json", m.authority.bytes)?;
        let checkpoint = read("checkpoint.json", m.checkpoint.bytes)?;
        if m.version != 1
            || m.sqlite != digest(&sqlite)
            || m.authority != digest(&authority)
            || m.checkpoint != digest(&checkpoint)
        {
            return Err(err(
                "KIO-E-LEDGER-BACKUP-MANIFEST-001",
                "backup digest mismatch",
            ));
        }
        Self::new(
            m.ledger_id,
            m.era,
            m.sequence,
            sqlite,
            authority,
            checkpoint,
        )
    }
}
fn put(s: &StoreDirectory, n: &str, b: &[u8]) -> Result<()> {
    s.write_atomic(Path::new(n), b, Publication::CreateOnly)
        .map_err(store_err)?;
    s.ensure_owner_private(Path::new(n)).map_err(store_err)
}
fn digest(b: &[u8]) -> DigestRecord {
    DigestRecord {
        bytes: b.len() as u64,
        sha256: Sha256::digest(b)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    }
}
fn err(c: &'static str, m: impl Into<String>) -> PipelineError {
    PipelineError::contract(c, m)
}
fn store_err(e: kio_core::KioError) -> PipelineError {
    err("KIO-E-LEDGER-BACKUP-PRIVATE-001", e.to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn backup() -> LedgerBackup {
        LedgerBackup::new(
            "ledger".to_owned(),
            "2026-09".to_owned(),
            7,
            b"sqlite".to_vec(),
            b"authority".to_vec(),
            b"checkpoint".to_vec(),
        )
        .expect("bounded backup")
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
        let temporary = tempfile::tempdir().expect("backup fixture container");
        let retained = StoreDirectory::open(temporary.path()).expect("fixture container");
        let _private = retained
            .create_directory(Path::new("private"))
            .expect("create private backup directory");
        PrivateTestDirectory {
            path: temporary.path().join("private"),
            _temporary: temporary,
        }
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
        assert_eq!(loaded.sqlite, backup.sqlite);
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
}
