//! A selected credential can become unsafe after the final paid admission.
//! The real backend counter distinguishes a rejected factory from a send.
use crate::commands::*;
use crate::context::AppContext;
use kio_core::control::ControlHandle;
use kio_core::test_control::{DebugTestControl, Selector, TestEnvGuard};
use kio_pipeline::ledger::ops::{
    cost_ledger_rows_for_key, get_batch_request, requests_with_intent_for_scope,
};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
enum Subject {
    Document,
    Image,
    Query,
}

fn index() -> CommandRequest {
    CommandRequest::Index(IndexArgs {
        preview: false,
        yes: false,
        online: false,
        offline: true,
        realtime: false,
        batch: false,
    })
}
fn resume() -> CommandRequest {
    CommandRequest::Batch(BatchArgs {
        command: Some(BatchCommand::Resume(ResumeArgs {
            recheck_budget: false,
            override_budget: false,
            online: true,
            offline: false,
            realtime: true,
            batch: false,
        })),
    })
}
fn query(text: &str) -> CommandRequest {
    CommandRequest::Search(SearchArgs {
        query: Some(text.into()),
        mode: Some(SearchMode::Vector),
        scope: None,
        descendants: false,
        all_scopes: false,
        at: None,
        all_history: false,
        include_deleted: false,
        since: None,
        limit: 20,
        offset: None,
        cursor: None,
        online: true,
        offline: false,
    })
}

fn run(subject: Subject, test_name: &str) {
    if !crate::contextual_embedding_tests::isolated_child_for(test_name) {
        return;
    }
    let _lock = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let _local = TestEnvGuard::remove("KIO_EVAL_DETERMINISTIC_EMBED");
    let _mock = TestEnvGuard::set("KIO_TEST_GEMINI_EMBED", "mock");
    let _markdown = TestEnvGuard::set("KIO_TEST_MARKDOWNIZE_ADAPTER", "deterministic");
    let config = PathBuf::from(std::env::var_os("XDG_CONFIG_HOME").unwrap()).join("kio");
    if !config.exists() {
        crate::test_support::create_private_test_directory(&config);
    }
    let tools = config.join("tools.toml");
    crate::test_support::create_private_test_file(&tools).write_all(
        b"[embedding.gemini_embedding_2]\nkind = 'online_api'\nauth = 'plain:synthetic-unsent-credential'\n"
    ).unwrap();
    let root = crate::test_support::PrivateTempDir::new();
    let interaction = Arc::new(crate::test_support::DiagnosticInteraction::new(None, false));
    let context = AppContext {
        working_directory: root.path().into(),
        interaction: interaction.clone(),
    };
    fs::write(
        root.path().join("control.md"),
        "# Positive control\n\nactual supported paid embedding fixture\n",
    )
    .unwrap();
    crate::execute(
        &context,
        CommandRequest::Init(InitArgs {
            path: None,
            independent: true,
            yes: true,
        }),
    )
    .unwrap();
    crate::execute(&context, index()).unwrap();
    crate::execute(
        &context,
        CommandRequest::Adapter(AdapterArgs {
            command: Some(AdapterCommand::Approve(ApproveArgs {
                target: AdapterTarget::Tool("gemini_embedding_2".into()),
                preview: false,
                yes: true,
                resume: None,
                send_secrets: false,
            })),
        }),
    )
    .unwrap();
    kio_adapter::catalog::reset_mock_embedding_dispatch_count();
    crate::execute(&context, resume()).unwrap();
    assert!(
        kio_adapter::catalog::mock_embedding_dispatch_count() > 0,
        "positive fixture must enter actual mock backend"
    );
    let base = ControlHandle::new(DebugTestControl::from_env());
    let repo = kio_core::scope::Repository::open_with_control(root.path(), base.clone()).unwrap();
    match subject {
        Subject::Document => {
            fs::write(
                root.path().join("fresh.md"),
                "# Fresh subject\n\nnew content needs a distinct paid document reservation\n",
            )
            .unwrap();
        }
        Subject::Image => {
            let mut config: toml::Value =
                toml::from_str(&repo.read_config_document().unwrap()).unwrap();
            config
                .as_table_mut()
                .unwrap()
                .entry("embedding")
                .or_insert_with(|| toml::Value::Table(Default::default()))
                .as_table_mut()
                .unwrap()
                .insert(
                    "external_send".into(),
                    toml::Value::String("text_only".into()),
                );
            repo.replace_config_value(config).unwrap();
            fs::write(
                root.path().join("fresh.png"),
                include_bytes!(
                    "../../../kio-cli/tests/fixtures/o2_golden/png-original-control.png"
                ),
            )
            .unwrap();
        }
        Subject::Query => {
            crate::execute(&context, query("positive paid query control")).unwrap();
        }
    }
    if !matches!(subject, Subject::Query) {
        crate::execute(&context, index()).unwrap();
    }
    if matches!(subject, Subject::Image) {
        // Complete generated Markdown before the image-only final barrier.
        crate::execute(&context, resume()).unwrap();
        let mut config: toml::Value =
            toml::from_str(&repo.read_config_document().unwrap()).unwrap();
        config["embedding"]["external_send"] = toml::Value::String("text_and_images".into());
        repo.replace_config_value(config).unwrap();
    }
    let settings =
        Arc::new(crate::settings::adapter_runtime_settings_from_user_config(&base).unwrap());
    let expected_settings =
        crate::settings::adapter_runtime_settings_from_user_config(&base).unwrap();
    let tasks_before = kio_pipeline::task::TaskStore::new(repo.kio_dir())
        .all()
        .unwrap();
    let cas_before = repo.object_store().embedding_hashes().unwrap();
    let scope_id = if matches!(subject, Subject::Query) {
        kio_pipeline::ledger::TaskKey::DEVICE_SCOPE_ID.to_owned()
    } else {
        repo.scope_identity().unwrap().scope_id
    };
    let ready = root.path().join("before-factory.ready");
    let mut debug = DebugTestControl::from_env();
    debug.core.durability_point =
        Selector::Known(kio_core::durability::DurabilityPoint::EmbeddingBeforeFactory);
    debug.core.durability_ready = Some(ready.clone());
    let control = ControlHandle::new(debug);
    let subject_request = if matches!(subject, Subject::Query) {
        query("fresh private paid query unsent canary")
    } else {
        resume()
    };
    kio_adapter::catalog::reset_mock_embedding_dispatch_count();
    let working_directory = root.path().to_path_buf();
    let worker = std::thread::spawn(move || {
        let context = AppContext {
            working_directory,
            interaction,
        };
        let locks = kio_core::scope::LockSession::default();
        crate::execute_dispatch(
            &crate::invocation::Invocation {
                context: &context,
                settings: &settings,
                control: &control,
                locks: &locks,
            },
            subject_request,
        )
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    while !ready.exists() && !worker.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let mutation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert!(
            ready.exists(),
            "{subject:?}: exact final-admission factory barrier must be reached"
        );
        let marker = fs::read_to_string(&ready).unwrap();
        assert!(marker.contains("point=embedding_before_factory"));
        assert!(marker.contains(&format!("pid={}", std::process::id())));
        let ledger = crate::ledger_access::open_optional_ledger_db(&base)
            .unwrap()
            .unwrap();
        let rows = requests_with_intent_for_scope(&ledger, &scope_id).unwrap();
        let rows: Vec<_> = rows
            .into_iter()
            .filter(|r| r.key.adapter_kind == "embedding")
            .collect();
        assert_eq!(
            rows.len(),
            1,
            "exact original reservation at factory boundary: {rows:?}"
        );
        let original = rows.into_iter().next().unwrap();
        assert!(original.estimated_usd > 0.0 && original.intent_token.is_some());
        assert!(
            cost_ledger_rows_for_key(&ledger, &original.key)
                .unwrap()
                .is_empty()
        );
        let job = kio_pipeline::ledger::embedding_job::embedding_job(&ledger, &original)
            .unwrap()
            .unwrap();
        drop(ledger);
        // Same bytes and declaration; only the selected credential's private
        // file identity becomes invalid. This portably forces factory refusal.
        fs::hard_link(&tools, config.join("credential-alias")).unwrap();
        let expected = crate::object_persistence::adapter_to_kio(
            expected_settings
                .role_credentials_ready("embedding")
                .unwrap_err(),
        );
        assert_eq!(
            expected.error_code(),
            kio_adapter::tool_lock::TOOLS_PRIVATE_CREDENTIAL_ERROR
        );
        (original, job, expected)
    }));
    fs::write(ready.with_extension("release"), b"release").unwrap();
    let result = worker.join().expect("owned app worker must not panic");
    let (original, original_job, expected_error) = match mutation {
        Ok(row) => row,
        Err(panic) => std::panic::resume_unwind(panic),
    };
    assert_eq!(
        kio_adapter::catalog::mock_embedding_dispatch_count(),
        0,
        "factory refusal must not invoke actual backend: {result:?}"
    );
    let ledger = crate::ledger_access::open_optional_ledger_db(&base)
        .unwrap()
        .unwrap();
    let row = get_batch_request(&ledger, &original.key).unwrap().unwrap();
    let costs = cost_ledger_rows_for_key(&ledger, &original.key).unwrap();
    assert_eq!(
        costs.len(),
        1,
        "one terminal audit for original reservation: {costs:?}"
    );
    assert_eq!(
        costs[0].usd, 0.0,
        "proven unsent {subject:?} cannot incur unknown estimated charge: {costs:?}; app={result:?}"
    );
    assert_eq!(
        costs[0].batch_job_id,
        original.intent_token.as_deref().unwrap()
    );
    assert!(!costs[0].estimated);
    assert_ne!(
        costs[0].outcome,
        kio_pipeline::ledger::Outcome::UnknownSettled
    );
    assert_eq!(costs[0].submission_seq, original.submission_seq);
    assert_eq!(row.submission_seq, original.submission_seq);
    assert!(
        row.intent_token.is_none(),
        "normal terminal cleanup completed"
    );
    assert!(
        row.upload_id.is_none() && row.batch_job_id.is_none() && row.provider_scope_id.is_none()
    );
    assert_eq!(
        row.job_create_started_at, original.job_create_started_at,
        "Sync reservation's local stale clock is unchanged"
    );
    let error = result.expect_err("selected unavailable credential must be reported");
    assert_eq!(error.error_code(), expected_error.error_code());
    assert_eq!(error.message(), expected_error.message());
    assert_eq!(error.context()["dependency"], "adapter_configuration");
    assert_eq!(error.context()["role"], "embedding");
    let mut cas_after = repo.object_store().embedding_hashes().unwrap();
    let mut cas_before = cas_before;
    cas_after.sort();
    cas_before.sort();
    assert_eq!(
        cas_after, cas_before,
        "unsent attempt cannot publish a new vector"
    );
    let tasks_after = kio_pipeline::task::TaskStore::new(repo.kio_dir())
        .all()
        .unwrap();
    for before in tasks_before.iter().filter(|task| {
        original_job
            .binding
            .members
            .iter()
            .any(|member| task.embedding_target.as_ref() == Some(&member.target))
    }) {
        let after = tasks_after
            .iter()
            .find(|task| task.task_id == before.task_id)
            .unwrap();
        assert_eq!(
            after.attempts, before.attempts,
            "unsent attempt must be refunded"
        );
        assert_eq!(after.status, kio_pipeline::task::TaskStatus::Pending);
        assert!(
            after.deadline.is_none()
                && after.heartbeat_at.is_none()
                && after.reservation_id.is_none()
        );
        assert!(after.reserved_usd.is_none() && after.reserved_month.is_none());
    }
}

#[test]
fn paid_document_factory_refusal_zero_closes_original_reservation() {
    run(
        Subject::Document,
        "embedding::paid_admission_tests::paid_document_factory_refusal_zero_closes_original_reservation",
    );
}
#[test]
fn paid_image_factory_refusal_zero_closes_original_reservation() {
    run(
        Subject::Image,
        "embedding::paid_admission_tests::paid_image_factory_refusal_zero_closes_original_reservation",
    );
}
#[test]
fn paid_query_factory_refusal_zero_closes_original_reservation() {
    run(
        Subject::Query,
        "embedding::paid_admission_tests::paid_query_factory_refusal_zero_closes_original_reservation",
    );
}
