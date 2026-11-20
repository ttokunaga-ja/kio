//! Durable, constant-sized ownership proof for private record runs.
use super::RecordStageRoot;
use crate::{
    Result,
    store_dir::{
        AtomicWorkspaceState, DirectoryStageIdentity, Publication, StageEntryState, StageIdentity,
        StageWriter, StoreDirectory,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::File,
    path::{Path, PathBuf},
};

const JOURNAL_BYTES: u64 = 16 * 1024;
const GATE: &str = ".record-gate";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u8,
    directory: DirectoryStageIdentity,
    records: StageIdentity,
    lease: StageIdentity,
}

pub(super) struct RecordResidue {
    pub(super) path: String,
    pub(super) reason: String,
}

pub(super) struct RunDirectory {
    pub(super) parent: StoreDirectory,
    pub(super) directory: StoreDirectory,
    pub(super) identity: DirectoryStageIdentity,
    pub(super) leaf: PathBuf,
    identities: StoreDirectory,
    journal: Journal,
    bytes: Vec<u8>,
    lease: File,
    root: RecordStageRoot,
}

fn child(parent: &StoreDirectory, name: &Path) -> Result<StoreDirectory> {
    let directory =
        StoreDirectory::from_retained(parent.open_directory(name)?, parent.path().join(name))?;
    crate::private_fs::verify_private_directory_handle(&directory)?;
    Ok(directory)
}

pub(super) fn container(
    parent: &StoreDirectory,
    name: &str,
    create: bool,
) -> Result<Option<StoreDirectory>> {
    let leaf = Path::new(name);
    if !parent.contains_entry(leaf)? {
        if !create {
            return Ok(None);
        }
        match parent.create_directory(leaf) {
            Ok(_) => {}
            Err(error)
                if error
                    .context()
                    .get("io_error_kind")
                    .and_then(serde_json::Value::as_str)
                    == Some("already_exists") => {}
            Err(error) => return Err(error),
        }
    }
    child(parent, leaf).map(Some)
}

fn containers(
    owner: &StoreDirectory,
    create: bool,
) -> Result<Option<(StoreDirectory, StoreDirectory)>> {
    let Some(stages) = container(owner, "stages", create)? else {
        return Ok(None);
    };
    let Some(identities) = container(owner, "identities", create)? else {
        return Ok(None);
    };
    Ok(Some((stages, identities)))
}

impl RunDirectory {
    pub(super) fn create(root: &RecordStageRoot) -> Result<(Self, StageWriter)> {
        let registry = root.registry(true)?.expect("created registry");
        let (parent, identities) = containers(&registry, true)?.expect("created containers");
        let gate = identities.lock_private_gate(Path::new(GATE))?;
        let mut nonce = [0; 32];
        getrandom::fill(&mut nonce).map_err(|error| {
            crate::KioError::io(error.to_string(), registry.path().display().to_string())
        })?;
        let leaf = PathBuf::from(format!("kio-records-{}", crate::cas::lower_hex(&nonce)));
        let (directory, identity) = parent.create_stage_directory(&leaf)?;
        // Every crash before journal publication leaves unknown residue, never
        // an invitation to infer ownership from a filename or a nonce.
        let lease = directory.create_private_file(Path::new("lease"))?;
        let lease_identity = StageIdentity::initialize(&lease)?;
        lease.try_lock().map_err(|error| {
            crate::KioError::schema(format!("cannot lock new record lease: {error}"))
        })?;
        let writer = StageWriter::create(&directory, Path::new("records"))?;
        let journal = Journal {
            version: 1,
            directory: identity.clone(),
            records: writer.identity().clone(),
            lease: lease_identity,
        };
        let bytes = serde_json::to_vec(&journal)
            .map_err(|error| crate::KioError::schema(error.to_string()))?;
        identities.write_atomic(&leaf, &bytes, Publication::CreateOnly)?;
        let location = Self {
            parent,
            directory,
            identity,
            leaf,
            identities,
            journal,
            bytes,
            lease,
            root: root.clone(),
        };
        // Explicit unlock matters when a fork has inherited the same handle.
        let _ = gate.unlock();
        Ok((location, writer))
    }
}

impl Drop for RunDirectory {
    fn drop(&mut self) {
        // No recursive/pathname fallback, even if a retained directory moved.
        if let Ok(gate) = self.identities.lock_private_gate(Path::new(GATE)) {
            if cleanup(
                &self.parent,
                &self.identities,
                &self.leaf,
                &self.directory,
                &self.journal,
                &self.bytes,
            )
            .is_err()
            {
                self.root.preserve_scratch();
            }
            let _ = gate.unlock();
        } else {
            self.root.preserve_scratch();
        }
        let _ = self.lease.unlock();
    }
}

fn named_directory_matches(
    parent: &StoreDirectory,
    leaf: &Path,
    identity: &DirectoryStageIdentity,
) -> Result<bool> {
    Ok(parent
        .inspect_stages(&[], &[(leaf.to_owned(), identity.clone())])?
        .into_iter()
        .any(|entry| entry.name == leaf.as_os_str() && entry.state == StageEntryState::Owned))
}

fn retire(identities: &StoreDirectory, leaf: &Path, bytes: &[u8]) -> Result<()> {
    identities.quarantine_then_remove(leaf, bytes, JOURNAL_BYTES)
}

fn cleanup(
    parent: &StoreDirectory,
    identities: &StoreDirectory,
    leaf: &Path,
    directory: &StoreDirectory,
    journal: &Journal,
    bytes: &[u8],
) -> Result<()> {
    journal.directory.verify(directory)?;
    if !named_directory_matches(parent, leaf, &journal.directory)? {
        return Err(crate::KioError::schema(
            "record run directory binding changed; preserved",
        ));
    }
    directory.discard_stage(Path::new("records"), &journal.records)?;
    directory.discard_stage(Path::new("lease"), &journal.lease)?;
    journal.directory.verify(directory)?;
    crate::platform::stage_operations::discard_empty_directory(
        parent,
        leaf,
        &directory.root_handle(),
    )?;
    parent.sync()?;
    retire(identities, leaf, bytes)
}

pub(super) fn inspect(root: &StoreDirectory, reclaim: bool) -> Result<Vec<RecordResidue>> {
    let Some((parent, identities)) = containers(root, false)? else {
        return Ok(Vec::new());
    };
    if reclaim {
        let gate = identities.lock_private_gate(Path::new(GATE))?;
        let result = inspect_containers(&parent, &identities, true);
        let _ = gate.unlock();
        result
    } else {
        // Read-only inventory must observe one registry state, including the
        // interval between run creation and durable journal publication. Open
        // only an existing gate: inspecting an unused registry creates nothing.
        let gate = if identities.contains_entry(Path::new(GATE))? {
            let gate = identities.open_regular_read(Path::new(GATE), 0)?;
            crate::private_fs::verify_private_regular_file_handle(&gate)?;
            gate.lock()
                .map_err(|error| crate::store_dir::ioerr(&identities.path().join(GATE), error))?;
            Some(gate)
        } else {
            None
        };
        let result = inspect_containers(&parent, &identities, false);
        if let Some(gate) = gate {
            let _ = gate.unlock();
        }
        result
    }
}

fn residue(
    out: &mut Vec<RecordResidue>,
    directory: &StoreDirectory,
    leaf: &Path,
    reason: impl Into<String>,
) {
    out.push(RecordResidue {
        path: directory.path().join(leaf).display().to_string(),
        reason: reason.into(),
    });
}

fn inspect_containers(
    parent: &StoreDirectory,
    identities: &StoreDirectory,
    reclaim: bool,
) -> Result<Vec<RecordResidue>> {
    let mut out = Vec::new();
    if reclaim {
        identities.recover_atomic(&[identities])?;
    } else if identities.inspect_atomic()? == AtomicWorkspaceState::Pending {
        residue(
            &mut out,
            identities,
            Path::new(crate::store_dir::ATOMIC_WORKSPACE_DIR),
            "interrupted record identity journal publication",
        );
    }
    let mut journals = BTreeMap::new();
    // Inventory through R1, including unsafe names, without adopting them.
    for entry in identities.inspect_stages(&[], &[])? {
        let leaf = PathBuf::from(entry.name);
        if leaf == Path::new(GATE) || leaf == Path::new(crate::store_dir::ATOMIC_WORKSPACE_DIR) {
            continue;
        }
        let parsed = (|| {
            if !leaf.to_str().is_some_and(valid_run_name) {
                return Err(crate::KioError::schema(
                    "unknown record identity journal name",
                ));
            }
            let bytes =
                crate::private_fs::read_private_file_at_path(identities, &leaf, JOURNAL_BYTES)?;
            let journal: Journal = serde_json::from_slice(&bytes)
                .map_err(|error| crate::KioError::schema(error.to_string()))?;
            if journal.version != 1 {
                return Err(crate::KioError::schema(
                    "unsupported record identity journal version",
                ));
            }
            Ok((journal, bytes))
        })();
        match parsed {
            Ok(saved) => {
                journals.insert(leaf, saved);
            }
            Err(error) => residue(&mut out, identities, &leaf, error.message()),
        }
    }
    let expected = journals
        .iter()
        .map(|(leaf, (journal, _))| (leaf.clone(), journal.directory.clone()))
        .collect::<Vec<_>>();
    for entry in parent.inspect_stages(&[], &expected)? {
        let leaf = PathBuf::from(entry.name);
        match entry.state {
            StageEntryState::Unidentified { reason } => residue(&mut out, parent, &leaf, reason),
            StageEntryState::Missing => {
                let (_, bytes) = &journals[&leaf];
                if reclaim {
                    retire(identities, &leaf, bytes)?;
                } else {
                    residue(
                        &mut out,
                        identities,
                        &leaf,
                        "record run absent; saved journal awaits retirement",
                    );
                }
            }
            StageEntryState::Owned => {
                let (journal, bytes) = &journals[&leaf];
                let inspected = (|| {
                    let directory = child(parent, &leaf)?;
                    journal.directory.verify(&directory)?;
                    // A crash after both unlinks but before rmdir leaves no
                    // lease to lock. Only an already-empty run can complete
                    // this retirement without a lease handle.
                    if !directory.contains_entry(Path::new("lease"))?
                        && directory.entries(Path::new(""))?.is_empty()
                    {
                        return inspect_dead(
                            &mut out,
                            (parent, identities),
                            &leaf,
                            &directory,
                            journal,
                            bytes,
                            reclaim,
                        );
                    }
                    // Inspect only the lease before touching payload metadata:
                    // active owners may deliberately replace their records.
                    let lease = directory.open_regular_read(Path::new("lease"), 0)?;
                    crate::private_fs::verify_private_regular_file_handle(&lease)?;
                    journal.lease.verify(&lease)?;
                    match lease.try_lock() {
                        Ok(()) => {}
                        Err(std::fs::TryLockError::WouldBlock) => return Ok(()),
                        Err(error) => {
                            return Err(crate::KioError::schema(format!(
                                "cannot inspect record lease: {error}"
                            )));
                        }
                    }
                    let result = inspect_dead(
                        &mut out,
                        (parent, identities),
                        &leaf,
                        &directory,
                        journal,
                        bytes,
                        reclaim,
                    );
                    let _ = lease.unlock();
                    result
                })();
                if let Err(error) = inspected {
                    residue(&mut out, parent, &leaf, error.message());
                }
            }
            StageEntryState::Captured { .. } => {
                unreachable!("directory expectations cannot be captures")
            }
        }
    }
    Ok(out)
}

fn inspect_dead(
    out: &mut Vec<RecordResidue>,
    owners: (&StoreDirectory, &StoreDirectory),
    leaf: &Path,
    directory: &StoreDirectory,
    journal: &Journal,
    bytes: &[u8],
    reclaim: bool,
) -> Result<()> {
    let (parent, identities) = owners;
    let entries = directory.inspect_stages(
        &[
            (PathBuf::from("records"), journal.records.clone()),
            (PathBuf::from("lease"), journal.lease.clone()),
        ],
        &[],
    )?;
    let mut unknown = false;
    for entry in entries {
        if let StageEntryState::Unidentified { reason } = entry.state {
            unknown = true;
            residue(out, directory, Path::new(&entry.name), reason);
        }
    }
    if unknown {
        return Ok(());
    }
    if reclaim {
        cleanup(parent, identities, leaf, directory, journal, bytes)?;
    } else {
        residue(
            out,
            parent,
            leaf,
            "interrupted private record stage; saved identity matches and lease is inactive",
        );
    }
    Ok(())
}

fn valid_run_name(name: &str) -> bool {
    name.strip_prefix("kio-records-").is_some_and(|suffix| {
        suffix.len() == 64
            && suffix
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk_records::test_store_root;

    fn inspect_root(root: &RecordStageRoot, reclaim: bool) -> Result<Vec<RecordResidue>> {
        match root.registry(false)? {
            Some(registry) => inspect(&registry, reclaim),
            None => Ok(Vec::new()),
        }
    }

    #[test]
    fn readonly_inspection_serializes_with_existing_gate_without_creating_root() {
        use std::{sync::mpsc, time::Duration};
        let fixture = tempfile::tempdir().unwrap();
        let root = test_store_root(&fixture);
        assert!(inspect_root(&root, false).unwrap().is_empty());
        assert!(!root.directory.path().join("internal").exists());
        let (run, writer) = RunDirectory::create(&root).unwrap();
        drop(writer);
        let gate = run.identities.lock_private_gate(Path::new(GATE)).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let inspected_root = root.clone();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            finished_tx
                .send(inspect_root(&inspected_root, false))
                .unwrap();
        });
        started_rx.recv().unwrap();
        let observed = finished_rx.recv_timeout(Duration::from_millis(100));
        gate.unlock().unwrap();
        assert!(
            matches!(observed, Err(mpsc::RecvTimeoutError::Timeout)),
            "inspection must wait for the registry mutation gate"
        );
        assert!(
            finished_rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap()
                .is_empty()
        );
        worker.join().unwrap();
        drop(run);
    }

    #[test]
    fn inspection_does_not_reclaim_an_inactive_run() {
        let fixture = tempfile::tempdir().unwrap();
        let root = test_store_root(&fixture);
        let (old, writer) = RunDirectory::create(&root).unwrap();
        drop(writer);
        old.lease.unlock().unwrap();
        {
            let _inspection = crate::disk_records::StageInspection::begin();
            let _new = crate::disk_records::DiskRecords::from_records(
                &root,
                0..=crate::disk_records::SORT_BUFFER_RECORDS,
            )
            .unwrap();
            assert!(old.parent.contains_entry(&old.leaf).unwrap());
            assert!(old.identities.contains_entry(&old.leaf).unwrap());
        }
        crate::disk_records::RecordWriter::<u64>::new(&root).unwrap();
        assert!(!old.parent.contains_entry(&old.leaf).unwrap());
        assert!(!old.identities.contains_entry(&old.leaf).unwrap());
    }

    #[test]
    fn independent_lease_handle_skips_active_run_before_payload_inspection() {
        let fixture = tempfile::tempdir().unwrap();
        let root = test_store_root(&fixture);
        let (run, writer) = RunDirectory::create(&root).unwrap();
        drop(writer);
        run.directory.remove_file(Path::new("records")).unwrap();
        run.directory
            .create_private_file(Path::new("records"))
            .unwrap();
        assert!(inspect_root(&root, false).unwrap().is_empty());
        assert!(inspect_root(&root, true).unwrap().is_empty());
        let records = run.directory.path().join("records");
        drop(run);
        assert!(records.exists(), "Drop must preserve replacement payload");
        assert!(!inspect_root(&root, false).unwrap().is_empty());
    }

    #[test]
    fn unknown_tagged_directory_is_reported_and_preserved() {
        let fixture = tempfile::tempdir().unwrap();
        let root = test_store_root(&fixture);
        let (run, writer) = RunDirectory::create(&root).unwrap();
        let parent = run.parent.clone();
        drop(writer);
        drop(run);
        let (unknown, _) = parent.create_stage_directory(Path::new("unknown")).unwrap();
        unknown.create_private_file(Path::new("keep")).unwrap();
        for reclaim in [false, true] {
            let result = inspect_root(&root, reclaim).unwrap();
            assert!(result.iter().any(|entry| entry.path.ends_with("unknown")));
            assert!(unknown.path().join("keep").exists());
        }
    }

    #[test]
    fn empty_interrupted_retirement_and_missing_run_retire_saved_journal() {
        for remove_directory in [false, true] {
            let fixture = tempfile::tempdir().unwrap();
            let root = test_store_root(&fixture);
            let (run, writer) = RunDirectory::create(&root).unwrap();
            drop(writer);
            run.directory
                .discard_stage(Path::new("records"), &run.journal.records)
                .unwrap();
            run.directory
                .discard_stage(Path::new("lease"), &run.journal.lease)
                .unwrap();
            if remove_directory {
                crate::platform::stage_operations::discard_empty_directory(
                    &run.parent,
                    &run.leaf,
                    &run.directory.root_handle(),
                )
                .unwrap();
            }
            assert!(!inspect_root(&root, false).unwrap().is_empty());
            assert!(inspect_root(&root, true).unwrap().is_empty());
            assert!(!run.identities.contains_entry(&run.leaf).unwrap());
            drop(run);
        }
    }
}
