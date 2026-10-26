//! APP-01: free resume/rebuild callers must not acquire device billing authority.
use crate::commands::{
    BatchArgs, BatchCommand, CommandRequest, IndexArgs, InitArgs, RepairArgs, RepairOperation,
    RepairRebuildDbArgs, ResumeArgs,
};
use crate::context::test_support::context;
use crate::test_support::PrivateTempDir;
use kio_core::control::ControlHandle;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Clone, Copy, Debug)]
enum Caller {
    Resume,
    Rebuild,
}
#[derive(Clone, Copy, Debug)]
enum Billing {
    Absent,
    Corrupt,
    Busy,
}

fn request(caller: Caller) -> CommandRequest {
    match caller {
        Caller::Resume => CommandRequest::Batch(BatchArgs {
            command: Some(BatchCommand::Resume(ResumeArgs {
                recheck_budget: false,
                override_budget: false,
                online: false,
                offline: false,
                realtime: false,
                batch: false,
            })),
        }),
        Caller::Rebuild => CommandRequest::Repair(RepairArgs {
            operation: RepairOperation::RebuildDb(RepairRebuildDbArgs {
                online: false,
                offline: false,
                realtime: false,
                batch: false,
            }),
        }),
    }
}

fn fixture() -> PrivateTempDir {
    let root = PrivateTempDir::new();
    fs::write(
        root.path().join("one.md"),
        "# Free local work\n\ncallervectorneedle\n",
    )
    .unwrap();
    crate::execute(
        &context(root.path()),
        CommandRequest::Init(InitArgs {
            path: None,
            independent: true,
            yes: true,
        }),
    )
    .unwrap();
    crate::execute(
        &context(root.path()),
        CommandRequest::Index(IndexArgs {
            preview: false,
            yes: true,
            online: false,
            offline: true,
            realtime: false,
            batch: false,
        }),
    )
    .unwrap();
    root
}

fn ledger_path() -> PathBuf {
    crate::ledger_access::ledger_db_path(&ControlHandle::default())
}
fn initialize_ledger() {
    drop(kio_pipeline::ledger::LedgerDb::initialize(ledger_path()).unwrap());
}

#[derive(Debug, PartialEq, Eq)]
struct Leaf {
    bytes: Vec<u8>,
    modified: SystemTime,
}
fn billing() -> BTreeMap<String, Leaf> {
    let parent = ledger_path().parent().unwrap().to_path_buf();
    if !parent.exists() {
        return BTreeMap::new();
    }
    fs::read_dir(parent)
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            if !(name.starts_with("cost-ledger.sqlite") || name == "ledger.lifecycle.lock") {
                return None;
            }
            let meta = fs::symlink_metadata(entry.path()).unwrap();
            assert!(meta.is_file() && !meta.file_type().is_symlink());
            Some((
                name,
                Leaf {
                    bytes: fs::read(entry.path()).unwrap(),
                    modified: meta.modified().unwrap(),
                },
            ))
        })
        .collect()
}

fn vectors(root: &Path) -> Vec<(String, String, Vec<f32>)> {
    let store = kio_core::cas::ObjectStore::new(root.join(".kio"));
    let mut values = store
        .embedding_hashes()
        .unwrap()
        .into_iter()
        .map(|hash| {
            let object = store.read_embedding(&hash).unwrap();
            assert!(object.vector.iter().all(|v| v.is_finite()));
            assert!(!object.vector.is_empty());
            (object.target_type, object.target_hash, object.vector)
        })
        .collect::<Vec<_>>();
    values.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    assert!(
        values.iter().any(|row| row.0 == "chunk"),
        "positive fixture needs actual chunk vectors"
    );
    // Observe the source projection as well as CAS bytes. A successful command
    // that loses its searchable vector projection is not a free local result.
    let source = rusqlite::Connection::open_with_flags(
        root.join(".kio/index/sqlite.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .unwrap();
    let count: i64 = source
        .query_row(
            "SELECT COUNT(*) FROM embeddings WHERE target_type='chunk'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(count > 0, "source must retain actual vector rows");
    values
}

fn check(caller: Caller, state: Billing) {
    initialize_ledger();
    let root = fixture();
    let expected = vectors(root.path());
    if matches!(state, Billing::Absent) {
        // Retire only this isolated child's known, freshly created fixture
        // artifacts after the positive index prerequisite has succeeded.
        let owned = billing();
        let parent = ledger_path().parent().unwrap().to_path_buf();
        for (name, expected) in owned {
            let path = parent.join(name);
            assert_eq!(fs::read(&path).unwrap(), expected.bytes);
            fs::remove_file(path).unwrap();
        }
        assert!(matches!(
            crate::ledger_access::open_search_budget_snapshot(
                &ledger_path(),
                &ControlHandle::default()
            ),
            Ok(None)
        ));
    }
    if matches!(state, Billing::Corrupt) {
        let directory =
            kio_core::private_fs::verify_private_directory(ledger_path().parent().unwrap())
                .unwrap();
        directory
            .write_atomic(
                Path::new("cost-ledger.sqlite"),
                b"APP01 malformed billing database\n",
                kio_core::store_dir::Publication::Replace,
            )
            .unwrap();
    }
    let before = billing();
    if matches!(state, Billing::Absent) {
        assert!(before.is_empty());
    }
    let held = if matches!(state, Billing::Busy) {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(
                ledger_path()
                    .parent()
                    .unwrap()
                    .join("ledger.lifecycle.lock"),
            )
            .unwrap();
        file.try_lock().unwrap();
        let contender = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(
                ledger_path()
                    .parent()
                    .unwrap()
                    .join("ledger.lifecycle.lock"),
            )
            .unwrap();
        assert!(matches!(
            contender.try_lock(),
            Err(fs::TryLockError::WouldBlock)
        ));
        Some((file, contender))
    } else {
        None
    };
    crate::execute(&context(root.path()), request(caller)).unwrap_or_else(|error| {
        panic!("{caller:?}/{state:?}: billing blocked free caller: {error}")
    });
    assert_eq!(vectors(root.path()), expected, "{caller:?}/{state:?}");
    let mut search = super::search_request(Some(crate::commands::SearchMode::Vector));
    search.query = Some("callervectorneedle".into());
    search.offline = true;
    let response = crate::execute(&context(root.path()), CommandRequest::Search(search)).unwrap();
    let payload = serde_json::to_value(response.body).unwrap();
    assert_eq!(payload["resolved_mode"], "vector", "{payload}");
    assert!(
        !payload["results"].as_array().unwrap().is_empty(),
        "{payload}"
    );
    drop(held);
    assert_eq!(
        billing(),
        before,
        "{caller:?}/{state:?}: billing artifacts changed"
    );
}

macro_rules! case {
    ($name:ident, $caller:ident, $state:ident) => {
        #[test]
        fn $name() {
            if !super::operation_preconditions_tests::isolated(concat!(
                "tests::caller_ledger_independence_tests::",
                stringify!($name)
            )) {
                return;
            }
            let _lock = kio_core::test_control::test_env_lock()
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            check(Caller::$caller, Billing::$state);
        }
    };
}
case!(
    app01_resume_absent_billing_preserves_vectors,
    Resume,
    Absent
);
case!(
    app01_resume_corrupt_billing_preserves_vectors,
    Resume,
    Corrupt
);
case!(app01_resume_busy_billing_preserves_vectors, Resume, Busy);
case!(
    app01_rebuild_absent_billing_preserves_vectors,
    Rebuild,
    Absent
);
case!(
    app01_rebuild_corrupt_billing_preserves_vectors,
    Rebuild,
    Corrupt
);
case!(app01_rebuild_busy_billing_preserves_vectors, Rebuild, Busy);
