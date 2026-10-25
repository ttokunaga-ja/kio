//! Below the CLI scope lock: revoke real device authority while its image owner lives.
use super::{GrantOperation, GrantState, PrivateGrantStore};
use crate::commands::{AdapterTarget, ApproveArgs, CommandRequest, IndexArgs, InitArgs};
use crate::context::AppContext;
use kio_adapter::tool_lock::AdapterRuntimeSettings;
use kio_core::control::ControlHandle;
use kio_core::test_control::{DebugTestControl, Selector, TestEnvGuard};
use kio_pipeline::ledger::TaskKey;
use kio_pipeline::ledger::ops::{cost_ledger_rows_for_key, get_batch_request};
use kio_pipeline::task::{EmbeddingTaskTarget, TaskStatus, TaskStore};
use std::fs;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
enum Change {
    Unchanged,
    Network,
    SendSecrets,
}

fn index_args(online: bool) -> IndexArgs {
    IndexArgs {
        preview: false,
        yes: true,
        online,
        offline: !online,
        realtime: online,
        batch: false,
    }
}

fn external_send(repo: &kio_core::scope::Repository, value: &str) {
    let mut config: toml::Value = toml::from_str(&repo.read_config_document().unwrap()).unwrap();
    config
        .as_table_mut()
        .unwrap()
        .entry("embedding")
        .or_insert_with(|| toml::Value::Table(Default::default()))
        .as_table_mut()
        .unwrap()
        .insert(
            "external_send".to_owned(),
            toml::Value::String(value.into()),
        );
    repo.replace_config_value(config).unwrap();
}

fn run(change: Change, test_name: &str) {
    if !crate::contextual_embedding_tests::isolated_child_for(test_name) {
        return;
    }
    // The isolated child runs exactly one test. Its global environment lock is
    // retained until every worker has joined and every selector is restored.
    let _lock = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let _local = TestEnvGuard::remove("KIO_EVAL_DETERMINISTIC_EMBED");
    let _gemini = TestEnvGuard::set("KIO_TEST_GEMINI_EMBED", "mock");
    let root = crate::test_support::PrivateTempDir::new();
    let interaction = Arc::new(crate::test_support::DiagnosticInteraction::new(None, false));
    let context = AppContext {
        working_directory: root.path().to_path_buf(),
        interaction: interaction.clone(),
    };
    let png: [u8; 68] = [
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 11, 73, 68, 65, 84, 120, 156, 99, 248, 15, 4, 0, 9,
        251, 3, 253, 251, 94, 107, 43, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];
    for path in ["api_secret.png", "public.png"] {
        fs::write(root.path().join(path), png).unwrap();
    }
    assert!(kio_pipeline::scan::classify_secret("api_secret.png").is_some());
    crate::execute(
        &context,
        CommandRequest::Init(InitArgs {
            path: None,
            independent: false,
            yes: false,
        }),
    )
    .unwrap();
    // Offline ingest creates immutable native ownership, rather than test rows.
    crate::execute(&context, CommandRequest::Index(index_args(false))).unwrap();
    let base_control = ControlHandle::new(DebugTestControl::from_env());
    let repo =
        kio_core::scope::Repository::open_with_control(root.path(), base_control.clone()).unwrap();
    let settings = Arc::new(AdapterRuntimeSettings::default());
    let tool = kio_adapter::catalog::adopted_embedding_profile().adapter_id;
    let locks = kio_core::scope::LockSession::default();
    let invocation = crate::invocation::Invocation {
        context: &context,
        settings: &settings,
        control: &base_control,
        locks: &locks,
    };
    crate::approvals::approve(
        &invocation,
        ApproveArgs {
            target: AdapterTarget::Tool(tool.clone()),
            preview: false,
            yes: true,
            resume: None,
            send_secrets: true,
        },
        &crate::approvals::ApprovalControl::default(),
    )
    .unwrap();
    external_send(&repo, "text_only");
    crate::execute(&context, CommandRequest::Index(index_args(true))).unwrap();
    external_send(&repo, "text_and_images");
    let image_hash = kio_core::cas::hash_bytes(&png);
    let head = repo.head_commit_hash().unwrap().unwrap();
    let instances = crate::historical_reindex::retained_history_instances(&repo, &head).unwrap();
    let policy = crate::search::scope::current_embedding_policy(&repo).unwrap();
    let authority =
        crate::image_authority::retained_image_authority(&repo, &instances, &policy).unwrap();
    assert!(authority.permits(&image_hash));
    assert!(
        authority.requires_secret_approval(&image_hash),
        "public alias must retain secret approval requirement"
    );
    assert!(instances.iter().any(|instance| {
        crate::historical_reindex::pinned_done_units(&repo, &instance.raw_hash, &instance.normalize)
            .unwrap()
            .iter()
            .any(|unit| unit.owned_image_hashes.contains(&image_hash))
    }));
    let task_before = TaskStore::new(repo.kio_dir())
        .all()
        .unwrap()
        .into_iter()
        .find(|task| {
            matches!(
                task.embedding_target,
                Some(EmbeddingTaskTarget::Image { .. })
            )
        })
        .expect("real image task");
    let Some(EmbeddingTaskTarget::Image { embedding_hash, .. }) =
        task_before.embedding_target.clone()
    else {
        unreachable!()
    };
    assert_ne!(task_before.status, TaskStatus::Done);
    let profile = kio_adapter::catalog::adopted_embedding_profile();
    let key = TaskKey::new(
        &repo.scope_identity().unwrap().scope_id,
        "embedding",
        &embedding_hash,
        &profile.tool_profile_hash,
    );
    let path = crate::approvals::store_path(&base_control);
    let before_grants = PrivateGrantStore::open_readonly(&path, &base_control)
        .unwrap()
        .unwrap()
        .records()
        .to_vec();
    let paired: Vec<_> = before_grants
        .iter()
        .filter(|r| r.binding.tool_id == tool && r.state == GrantState::Active)
        .cloned()
        .collect();
    assert_eq!(paired.len(), 2);
    assert_eq!(paired[0].approval_id, paired[1].approval_id);
    let ready = root.path().join("image-reserved.ready");
    let trace = root.path().join("image-dispatch.jsonl");
    let mut debug = DebugTestControl::from_env();
    debug.core.durability_point =
        Selector::Known(kio_core::durability::DurabilityPoint::EmbeddingReserved);
    debug.core.durability_ready = Some(ready.clone());
    debug.cli.task_embed_trace = Some(trace.clone());
    let control = ControlHandle::new(debug);
    // The real app index keeps its writer authority during the reserve barrier.
    let root_path = root.path().to_path_buf();
    let worker_control = control.clone();
    let worker = std::thread::spawn(move || {
        let context = AppContext {
            working_directory: root_path,
            interaction,
        };
        let locks = kio_core::scope::LockSession::default();
        crate::execute_dispatch(
            &crate::invocation::Invocation {
                context: &context,
                settings: &settings,
                control: &worker_control,
                locks: &locks,
            },
            CommandRequest::Index(index_args(true)),
        )
    });
    // Native Windows may spend longer opening the fixture before reservation.
    // Keep a bounded arrival wait; the dispatch assertions remain unchanged.
    let deadline = Instant::now() + Duration::from_secs(60);
    while !ready.exists() && !worker.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let reached = ready.exists();
    let mutation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert!(
            reached,
            "scaffold failure: image reservation barrier not reached"
        );
        assert!(
            fs::read_to_string(&ready)
                .unwrap()
                .contains("point=embedding_reserved")
        );
        if !matches!(change, Change::Unchanged) {
            let operation = if matches!(change, Change::Network) {
                GrantOperation::Network
            } else {
                GrantOperation::SendSecrets
            };
            let mut store = PrivateGrantStore::open_or_create(&path, &base_control).unwrap();
            let revoked = store
                .revoke_matching("2026-10-20T00:00:00Z".into(), |binding| {
                    binding.tool_id == tool && binding.operation == operation
                })
                .unwrap();
            assert_eq!(
                revoked.len(),
                1,
                "only selected member of real pair revoked"
            );
            for original in &paired {
                let current = store
                    .records()
                    .iter()
                    .find(|r| r.grant_id == original.grant_id)
                    .unwrap();
                if original.binding.operation == operation {
                    assert_eq!(current.state, GrantState::Revoked);
                } else {
                    assert_eq!(current, original, "other paired authority unchanged");
                }
            }
        }
    }));
    // Always release and join before an assertion can unwind the fixture.
    fs::write(ready.with_extension("release"), b"release").unwrap();
    let result = worker.join().expect("image index worker must not panic");
    if let Err(panic) = mutation {
        std::panic::resume_unwind(panic);
    }
    let sends: Vec<serde_json::Value> = fs::read_to_string(&trace)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    // First behavioral assertion: direct adapter-edge observable, before later
    // projection/publication outcomes can disguise a forbidden physical send.
    assert_eq!(
        sends.len(),
        usize::from(matches!(change, Change::Unchanged)),
        "reserve/send authority race {change:?}: {sends:?}"
    );
    let ledger = crate::ledger_access::open_optional_ledger_db(&base_control)
        .unwrap()
        .unwrap();
    let row = get_batch_request(&ledger, &key)
        .unwrap()
        .expect("reserved original key");
    let charges = cost_ledger_rows_for_key(&ledger, &key).unwrap();
    let sqlite_path = crate::index::chunk_ledger::sqlite_path(repo.kio_dir())
        .canonicalize()
        .unwrap();
    let raw_path = sqlite_path.to_str().unwrap();
    let normalized = raw_path
        .strip_prefix(r"\\?\")
        .unwrap_or(raw_path)
        .replace('\\', "/");
    let absolute = if normalized.as_bytes().get(1) == Some(&b':') {
        format!("/{normalized}")
    } else {
        normalized
    };
    let mut encoded = String::new();
    for byte in absolute.bytes() {
        if matches!(byte, b'%' | b'?' | b'#' | b' ') || !byte.is_ascii() {
            use std::fmt::Write as _;
            write!(&mut encoded, "%{byte:02X}").unwrap();
        } else {
            encoded.push(char::from(byte));
        }
    }
    let connection = rusqlite::Connection::open_with_flags(
        format!("file:{encoded}?mode=ro&immutable=1"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .unwrap();
    let projected: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM embeddings WHERE target_type = 'image' AND target_id = ?1",
            [&image_hash],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(projected, i64::from(matches!(change, Change::Unchanged)));
    drop(connection);
    let tasks = TaskStore::new(repo.kio_dir()).all().unwrap();
    let task = tasks
        .iter()
        .find(|task| task.task_id == task_before.task_id)
        .unwrap();
    if matches!(change, Change::Unchanged) {
        result.unwrap();
        assert_eq!(sends[0]["input_type"], "image");
        assert_eq!(sends[0]["member_ids"], serde_json::json!([image_hash]));
        assert!(sends[0]["intent_token"].as_str().is_some());
        assert_eq!(charges.len(), 1);
        assert_eq!(charges[0].outcome, kio_pipeline::ledger::Outcome::Succeeded);
        assert_eq!(charges[0].submission_seq, row.submission_seq);
        assert_eq!(task.status, TaskStatus::Done);
        assert!(repo.object_store().read_embedding(&embedding_hash).is_ok());
    } else {
        // Command may report policy/auth hold; completion is not the oracle.
        let _ = result;
        assert_eq!(
            charges.len(),
            1,
            "original reservation must be zero-settled"
        );
        assert_eq!(charges[0].usd, 0.0);
        assert!(!charges[0].estimated);
        assert_ne!(charges[0].outcome, kio_pipeline::ledger::Outcome::Succeeded);
        assert_eq!(charges[0].submission_seq, row.submission_seq);
        assert_ne!(task.status, TaskStatus::Done);
        assert!(
            task.deadline.is_none() && task.heartbeat_at.is_none() && task.reservation_id.is_none(),
            "no live task claim after zero settlement"
        );
        assert!(repo.object_store().read_embedding(&embedding_hash).is_err());
        assert!(
            row.intent_token.is_none(),
            "no live ledger publication claim"
        );
    }
}

#[test]
fn o3_image_reserved_unchanged_dispatches_once() {
    run(
        Change::Unchanged,
        "grants::reserve_send_tests::o3_image_reserved_unchanged_dispatches_once",
    );
}
#[test]
fn o3_image_reserved_network_revoked_prevents_dispatch() {
    run(
        Change::Network,
        "grants::reserve_send_tests::o3_image_reserved_network_revoked_prevents_dispatch",
    );
}
#[test]
fn o3_image_reserved_send_secrets_revoked_prevents_dispatch() {
    run(
        Change::SendSecrets,
        "grants::reserve_send_tests::o3_image_reserved_send_secrets_revoked_prevents_dispatch",
    );
}
