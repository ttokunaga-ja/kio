//! A free index cannot retire a paid claim whose billing authority is unavailable.
use crate::commands::{CommandRequest, IndexArgs, InitArgs};
use crate::context::test_support::context;
use crate::test_support::{
    PrivateTempDir, create_private_test_directory, create_private_test_file,
};
use kio_core::control::ControlHandle;
use kio_core::test_control::TestEnvGuard;
use kio_pipeline::task::{TaskStatus, TaskStore, TaskType};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;

fn offline_index(root: &Path) -> crate::response::CommandResponse {
    crate::execute(
        &context(root),
        CommandRequest::Index(IndexArgs {
            preview: false,
            yes: true,
            online: false,
            offline: true,
            realtime: false,
            batch: false,
        }),
    )
    .unwrap()
}

#[derive(Debug, PartialEq, Eq)]
struct BillingLeaf {
    bytes: Vec<u8>,
    modified: std::time::SystemTime,
    created: Option<std::time::SystemTime>,
    #[cfg(unix)]
    identity: (u64, u64, u64, u32, u32, u32, i64, i64),
}

fn device_bytes(root: &Path) -> BTreeMap<std::path::PathBuf, BillingLeaf> {
    fn visit(
        root: &Path,
        directory: &Path,
        leaves: &mut BTreeMap<std::path::PathBuf, BillingLeaf>,
    ) {
        for entry in fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            if metadata.is_dir() {
                visit(root, &path, leaves);
                continue;
            }
            let relative = path.strip_prefix(root).unwrap();
            let billing = relative.components().any(|part| {
                let name = part.as_os_str().to_string_lossy();
                name.starts_with("cost-ledger") || name.starts_with("ledger.")
            });
            if !billing {
                continue;
            }
            assert!(
                metadata.is_file(),
                "billing leaf shape changed: {}",
                path.display()
            );
            #[cfg(unix)]
            let identity = {
                use std::os::unix::fs::MetadataExt;
                (
                    metadata.dev(),
                    metadata.ino(),
                    metadata.nlink(),
                    metadata.mode(),
                    metadata.uid(),
                    metadata.gid(),
                    metadata.ctime(),
                    metadata.ctime_nsec(),
                )
            };
            leaves.insert(
                relative.to_path_buf(),
                BillingLeaf {
                    bytes: fs::read(&path).unwrap(),
                    modified: metadata.modified().unwrap(),
                    created: metadata.created().ok(),
                    #[cfg(unix)]
                    identity,
                },
            );
        }
    }
    let mut leaves = BTreeMap::new();
    visit(root, &root.join("kio"), &mut leaves);
    leaves
}

#[test]
fn changed_local_input_preserves_paid_stamp_when_budget_is_unknown() {
    const TEST: &str = "tests::budget_retirement_preconditions_tests::changed_local_input_preserves_paid_stamp_when_budget_is_unknown";
    if !crate::contextual_embedding_tests::isolated_child_for(TEST) {
        return;
    }
    let _lock = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    const PNG: &[u8] = include_bytes!(
        "../../kio-eval/acceptance-fixtures/v1/authenticated-local-public/image.png"
    );
    for corrupt in [false, true] {
        let root = PrivateTempDir::new();
        fs::write(root.path().join("image.png"), PNG).unwrap();
        crate::execute(
            &context(root.path()),
            CommandRequest::Init(InitArgs {
                path: None,
                independent: true,
                yes: true,
            }),
        )
        .unwrap();
        offline_index(root.path());
        let store = TaskStore::new(root.path().join(".kio"));
        let task = store
            .all()
            .unwrap()
            .into_iter()
            .find(|task| task.task_type == TaskType::Markdownize && task.input_path == "image.png")
            .expect("image fixture must create a real online enrichment task");
        store
            .update_matching(|current| {
                if current.task_id != task.task_id {
                    return false;
                }
                current.status = TaskStatus::Pending;
                current.fallback_reason = Some("rate_limit".to_owned());
                current.hold_reason = None;
                current.reserved_usd = Some(0.25);
                current.reserved_month = Some("2026-10".to_owned());
                current.reservation_id = Some("019a0e00-0000-7000-8000-000000000001".to_owned());
                true
            })
            .unwrap();
        let before_task = store
            .all()
            .unwrap()
            .into_iter()
            .find(|row| row.task_id == task.task_id)
            .unwrap();
        let mut changed = PNG.to_vec();
        changed.extend_from_slice(b"changed input after paid reservation");
        fs::write(root.path().join("image.png"), changed).unwrap();
        let device = PrivateTempDir::new();
        create_private_test_directory(&device.path().join("kio"));
        let _data = TestEnvGuard::set("XDG_DATA_HOME", device.path());
        if corrupt {
            create_private_test_file(&crate::ledger_access::ledger_db_path(
                &ControlHandle::default(),
            ))
            .write_all(b"corrupt billing authority; never repair\n")
            .unwrap();
        }
        let before_device = device_bytes(device.path());
        let response = offline_index(root.path());
        let value = serde_json::to_value(response.body).unwrap();
        assert_eq!(value["budget_status"], "unknown", "{value}");
        let after_task = store
            .all()
            .unwrap()
            .into_iter()
            .find(|row| row.task_id == task.task_id)
            .unwrap();
        assert!(
            store
                .all()
                .unwrap()
                .iter()
                .any(|row| row.task_type == TaskType::Markdownize
                    && row.input_path == "image.png"
                    && row.input_hash != before_task.input_hash),
            "changed valid input must reach enqueue, not vacuously preserve the old task"
        );
        assert_eq!(
            after_task, before_task,
            "unavailable billing cannot retire a paid claim"
        );
        assert_eq!(
            device_bytes(device.path()),
            before_device,
            "free index changed billing artifacts"
        );
    }
}
