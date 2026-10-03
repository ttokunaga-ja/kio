use std::cell::Cell;

use super::*;
use kio_adapter::batch_client::{BatchJobRecord, BatchOutputLine, BatchUploadRecord};
use kio_adapter::gemini_batch_client::{
    GeminiBatchClient, GeminiBatchEmbedInput, GeminiBatchJobRecord, GeminiBatchPoll,
    GeminiBatchState, batch_display_name,
};
use kio_pipeline::ledger::ops::{cost_ledger_rows_for_key, phase1_intent};

fn canonical_tempdir() -> crate::test_support::PrivateTempDir {
    crate::test_support::PrivateTempDir::new()
}

fn unreadable() -> kio_adapter::AdapterError {
    kio_adapter::AdapterError::Network("synthetic unavailable".into())
}

struct GeminiClient {
    scope: Option<String>,
    record: GeminiBatchJobRecord,
    polls: Cell<usize>,
}

impl GeminiBatchClient for GeminiClient {
    fn provider_scope_id(&self) -> kio_adapter::Result<String> {
        self.scope.clone().ok_or_else(unreadable)
    }
    fn create_embedding_job(
        &self,
        _: &str,
        _: u32,
        _: &str,
        _: &[GeminiBatchEmbedInput],
    ) -> kio_adapter::Result<GeminiBatchJobRecord> {
        panic!("recovery must not create jobs")
    }
    fn poll_job(&self, name: &str) -> kio_adapter::Result<GeminiBatchPoll> {
        assert_eq!(name, "batches/recorded");
        self.polls.set(self.polls.get() + 1);
        Ok(GeminiBatchPoll {
            record: self.record.clone(),
            results: Some(Vec::new()),
        })
    }
    fn list_jobs(&self) -> kio_adapter::Result<Vec<GeminiBatchJobRecord>> {
        panic!("known-job recovery must not list")
    }
}

fn reserved_row(
    ledger: &LedgerDb,
    adapter: &str,
    profile: &str,
    scope: Option<&str>,
) -> BatchRequestRow {
    let key = LedgerTaskKey::new("scope", adapter, hash_bytes(b"input"), profile);
    let intent = phase1_intent(ledger, &key, RequestKind::Batch, 1.0, None).unwrap();
    if let Some(scope) = scope {
        phase2a_record_provider_scope(ledger, &key, &intent.intent_token, scope).unwrap();
    }
    phase2b_record_job_created(ledger, &key, &intent.intent_token, "batches/recorded").unwrap();
    get_batch_request(ledger, &key).unwrap().unwrap()
}

#[test]
fn gemini_collection_requires_scope_profile_and_returned_job_attribution() {
    for case in [
        "match",
        "missing_scope",
        "wrong_scope",
        "unreadable_scope",
        "wrong_profile",
        "wrong_job",
        "wrong_token",
    ] {
        let root = canonical_tempdir();
        let ledger = LedgerDb::initialize(root.path().join("device/ledger.sqlite")).unwrap();
        let row = reserved_row(
            &ledger,
            EMBEDDING_ADAPTER_KIND,
            "profile",
            (case != "missing_scope").then_some("gemini:project"),
        );
        let before_charges = cost_ledger_rows_for_key(&ledger, &row.key).unwrap();
        let client = GeminiClient {
            scope: if case == "unreadable_scope" {
                None
            } else {
                Some(
                    if case == "wrong_scope" {
                        "other"
                    } else {
                        "gemini:project"
                    }
                    .into(),
                )
            },
            record: GeminiBatchJobRecord {
                name: if case == "wrong_job" {
                    "batches/foreign"
                } else {
                    "batches/recorded"
                }
                .into(),
                display_name: batch_display_name(if case == "wrong_token" {
                    "foreign-token"
                } else {
                    row.intent_token.as_deref().unwrap()
                }),
                state: GeminiBatchState::Succeeded,
                responses_file: None,
            },
            polls: Cell::new(0),
        };
        let poll = poll_attributed_embedding_batch_job(
            &client,
            &row,
            Some(if case == "wrong_profile" {
                "changed-profile"
            } else {
                "profile"
            }),
        );
        assert_eq!(poll.is_some(), case == "match", "{case}");
        assert_eq!(
            client.polls.get(),
            usize::from(matches!(case, "match" | "wrong_job" | "wrong_token")),
            "{case}"
        );
        assert_eq!(
            get_batch_request(&ledger, &row.key).unwrap().unwrap(),
            row,
            "{case} must preserve reservations"
        );
        assert_eq!(
            cost_ledger_rows_for_key(&ledger, &row.key).unwrap(),
            before_charges
        );
    }
}

struct MistralClient {
    scope: Option<String>,
    record: BatchJobRecord,
    polls: Cell<usize>,
    fetches: Cell<usize>,
    deletes: Cell<usize>,
}

impl MistralBatchClient for MistralClient {
    fn provider_scope_id(&self) -> kio_adapter::Result<String> {
        self.scope.clone().ok_or_else(unreadable)
    }
    fn upload_batch_input(&self, _: &[u8], _: &str) -> kio_adapter::Result<String> {
        panic!("recovery must not upload")
    }
    fn create_job(&self, _: &str, _: &str, _: &Value) -> kio_adapter::Result<BatchJobRecord> {
        panic!("recovery must not create")
    }
    fn get_job(&self, job_id: &str) -> kio_adapter::Result<BatchJobRecord> {
        assert_eq!(job_id, "batches/recorded");
        self.polls.set(self.polls.get() + 1);
        Ok(self.record.clone())
    }
    fn list_jobs(&self) -> kio_adapter::Result<Vec<BatchJobRecord>> {
        panic!("known-job recovery must not list")
    }
    fn list_uploads(&self) -> kio_adapter::Result<Vec<BatchUploadRecord>> {
        panic!("known upload cleanup must not list")
    }
    fn delete_upload(&self, upload_id: &str) -> kio_adapter::Result<()> {
        assert_eq!(upload_id, "upload-recorded");
        self.deletes.set(self.deletes.get() + 1);
        Ok(())
    }
    fn fetch_output(&self, output_file_id: &str) -> kio_adapter::Result<Vec<BatchOutputLine>> {
        assert_eq!(output_file_id, "output-recorded");
        self.fetches.set(self.fetches.get() + 1);
        Err(unreadable())
    }
}

fn mistral_client(row: &BatchRequestRow) -> MistralClient {
    MistralClient {
        scope: Some("mistral:workspace".into()),
        record: BatchJobRecord {
            job_id: "batches/recorded".into(),
            status: BatchJobStatus::Success,
            output_file_id: Some("output-recorded".into()),
            metadata: json!({
                "intent_token": row.intent_token,
                "scope_id": row.key.scope_id,
                "adapter_kind": row.key.adapter_kind,
                "input_hash": row.key.input_hash,
                "tool_profile_hash": row.key.tool_profile_hash,
            }),
        },
        polls: Cell::new(0),
        fetches: Cell::new(0),
        deletes: Cell::new(0),
    }
}

fn markdown_task(row: &BatchRequestRow) -> TaskDescriptor {
    TaskDescriptor {
        task_id: "task_recovery".into(),
        task_type: TaskType::Markdownize,
        mode: Some(MarkdownizeMode::Full),
        input_path: "report.pdf".into(),
        input_hash: row.key.input_hash.clone(),
        previous_raw_hash: None,
        parent_run_id: None,
        changed_unit_keys: Vec::new(),
        output_ref: "online:mistral_ocr_markdownize".into(),
        unit_keys: None,
        status: TaskStatus::Pending,
        attempts: 0,
        next_retry_at: None,
        deadline: None,
        heartbeat_at: None,
        fallback_reason: None,
        created_at: "2026-09-26T00:00:00Z".into(),
        bbox_annotation_enabled: Some(false),
        hold_reason: None,
        reserved_usd: None,
        reserved_month: None,
        reservation_id: None,
    }
}

#[test]
fn mistral_collection_holds_unbound_scope_profile_and_job_without_effects() {
    for status in [BatchJobStatus::Success, BatchJobStatus::Failed] {
        for case in [
            "missing_scope",
            "wrong_scope",
            "unreadable_scope",
            "wrong_profile",
            "wrong_job",
            "wrong_token",
            "scope_id",
            "adapter_kind",
            "input_hash",
            "tool_profile_hash",
            "missing_key",
            "malformed_key",
        ] {
            let root = canonical_tempdir();
            let repo = Repository::init(root.path()).unwrap();
            let ledger = LedgerDb::initialize(root.path().join("device/ledger.sqlite")).unwrap();
            let row = reserved_row(
                &ledger,
                "markdownize",
                "profile",
                (case != "missing_scope").then_some("mistral:workspace"),
            );
            let mut client = mistral_client(&row);
            client.record.status = status.clone();
            match case {
                "wrong_scope" => client.scope = Some("other".into()),
                "unreadable_scope" => client.scope = None,
                "wrong_job" => client.record.job_id = "foreign".into(),
                "wrong_token" => client.record.metadata["intent_token"] = json!("foreign"),
                "scope_id" | "adapter_kind" | "input_hash" | "tool_profile_hash" => {
                    client.record.metadata[case] = json!("foreign")
                }
                "missing_key" => {
                    client
                        .record
                        .metadata
                        .as_object_mut()
                        .unwrap()
                        .remove("input_hash");
                }
                "malformed_key" => client.record.metadata["input_hash"] = json!(4),
                _ => {}
            }
            let mut profile = standard_online_markdownize_profile();
            profile.tool_profile_hash = if case == "wrong_profile" {
                "changed-profile"
            } else {
                "profile"
            }
            .into();
            let task = markdown_task(&row);
            let store = TaskStore::new(repo.kio_dir());
            store.append(&task).unwrap();
            let task_bytes = fs::read(repo.kio_dir().join("tasks.jsonl")).unwrap();
            let charges = cost_ledger_rows_for_key(&ledger, &row.key).unwrap();
            let result = poll_one_batch_markdownize_row(
                &repo,
                &store,
                &ledger,
                &client,
                &row,
                &profile,
                &task.output_ref,
                std::slice::from_ref(&task),
            )
            .unwrap();
            assert!(matches!(result, BatchPollDisposition::InFlight), "{case}");
            let expected_poll = usize::from(!matches!(
                case,
                "missing_scope" | "wrong_scope" | "unreadable_scope" | "wrong_profile"
            ));
            assert_eq!(
                (
                    client.polls.get(),
                    client.fetches.get(),
                    client.deletes.get()
                ),
                (expected_poll, 0, 0),
                "{case}"
            );
            assert_eq!(
                get_batch_request(&ledger, &row.key).unwrap().unwrap(),
                row,
                "{case}"
            );
            assert_eq!(
                cost_ledger_rows_for_key(&ledger, &row.key).unwrap(),
                charges,
                "{case}"
            );
            assert_eq!(
                fs::read(repo.kio_dir().join("tasks.jsonl")).unwrap(),
                task_bytes,
                "{case} must not persist task state"
            );
        }
    }
}

#[test]
fn mistral_matching_job_fetches_output_and_matching_failure_settles_and_cleans() {
    for failed in [false, true] {
        let root = canonical_tempdir();
        let repo = Repository::init(root.path()).unwrap();
        let ledger = LedgerDb::initialize(root.path().join("device/ledger.sqlite")).unwrap();
        let mut row = reserved_row(&ledger, "markdownize", "profile", Some("mistral:workspace"));
        phase2a_record_upload_id(
            &ledger,
            &row.key,
            row.intent_token.as_deref().unwrap(),
            "upload-recorded",
        )
        .unwrap();
        row = get_batch_request(&ledger, &row.key).unwrap().unwrap();
        let mut client = mistral_client(&row);
        if failed {
            client.record.status = BatchJobStatus::Failed;
        }
        let mut profile = standard_online_markdownize_profile();
        profile.tool_profile_hash = "profile".into();
        let task = markdown_task(&row);
        let store = TaskStore::new(repo.kio_dir());
        store.append(&task).unwrap();
        let result = poll_one_batch_markdownize_row(
            &repo,
            &store,
            &ledger,
            &client,
            &row,
            &profile,
            &task.output_ref,
            std::slice::from_ref(&task),
        )
        .unwrap();
        assert_eq!(
            (
                client.polls.get(),
                client.fetches.get(),
                client.deletes.get()
            ),
            (1, usize::from(!failed), usize::from(failed))
        );
        if failed {
            assert!(matches!(result, BatchPollDisposition::FailedPermanent));
            assert!(
                get_batch_request(&ledger, &row.key)
                    .unwrap()
                    .unwrap()
                    .intent_token
                    .is_none()
            );
            assert_eq!(
                cost_ledger_rows_for_key(&ledger, &row.key).unwrap().len(),
                1
            );
        } else {
            assert!(matches!(result, BatchPollDisposition::InFlight));
            assert_eq!(get_batch_request(&ledger, &row.key).unwrap().unwrap(), row);
        }
    }
}

#[test]
fn mistral_cleanup_requires_exact_scope_but_finishes_pre_upload_cancellation_locally() {
    for case in [
        "match",
        "wrong_scope",
        "unreadable_scope",
        "missing_scope",
        "pre_upload",
        "unrecorded_upload",
    ] {
        let root = canonical_tempdir();
        let ledger = LedgerDb::initialize(root.path().join("device/ledger.sqlite")).unwrap();
        let row = reserved_row(
            &ledger,
            "markdownize",
            "profile",
            (!matches!(case, "missing_scope" | "pre_upload")).then_some("mistral:workspace"),
        );
        recovery_settle_unknown(
            &ledger,
            &row.key,
            row.intent_token.as_deref().unwrap(),
            1.0,
            false,
        )
        .unwrap();
        let before = get_batch_request(&ledger, &row.key).unwrap().unwrap();
        let charges = cost_ledger_rows_for_key(&ledger, &row.key).unwrap();
        let mut client = mistral_client(&row);
        if matches!(case, "unreadable_scope" | "pre_upload") {
            client.scope = None;
        }
        if case == "wrong_scope" {
            client.scope = Some("foreign".into());
        }
        let upload =
            (!matches!(case, "pre_upload" | "unrecorded_upload")).then_some("upload-recorded");
        cleanup_batch_residue(
            &ledger,
            &client,
            &row.key,
            row.intent_token.as_deref().unwrap(),
            upload,
            row.provider_scope_id.as_deref(),
        )
        .unwrap();
        let after = get_batch_request(&ledger, &row.key).unwrap().unwrap();
        assert_eq!(
            (
                client.polls.get(),
                client.fetches.get(),
                client.deletes.get()
            ),
            (0, 0, usize::from(case == "match")),
            "{case}"
        );
        if matches!(case, "match" | "pre_upload") {
            let mut expected = before;
            expected.intent_token = None;
            assert_eq!(after, expected, "{case}");
        } else {
            assert_eq!(after, before, "{case}");
        }
        assert_eq!(
            cost_ledger_rows_for_key(&ledger, &row.key).unwrap(),
            charges
        );
    }
}

#[test]
fn credential_or_device_key_rotation_holds_existing_rows_without_provider_effects() {
    use kio_adapter::batch_client::EnvMistralBatchClient;
    use kio_adapter::batch_recovery::BatchRecoveryContext;
    use kio_adapter::gemini_batch_client::EnvGeminiBatchClient;
    use kio_adapter::tool_lock::{
        AdapterRuntimeSettings, declared_adapter_for_role, with_runtime_settings,
    };
    use kio_core::store_dir::{Publication, StoreDirectory, restrict_new_private_directory};
    let _guard = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let root = canonical_tempdir();
    let base = root.path().canonicalize().unwrap();
    let parent = StoreDirectory::open(&base).unwrap();
    let directory = parent.create_directory(Path::new("credentials")).unwrap();
    restrict_new_private_directory(&directory).unwrap();
    let store = StoreDirectory::from_retained(directory, base.join("credentials")).unwrap();
    let source = "[markdown]\nauth = \"env:KIO_TEST_RECOVERY_ROTATING_KEY\"\n[embedding]\nauth = \"env:KIO_TEST_RECOVERY_ROTATING_KEY\"\n";
    store
        .write_atomic(
            Path::new("tools.toml"),
            source.as_bytes(),
            Publication::CreateOnly,
        )
        .unwrap();
    let parsed: toml::Value = toml::from_str(source).unwrap();
    let settings = AdapterRuntimeSettings {
        declarations: ["markdown", "embedding"]
            .into_iter()
            .map(|role| {
                (
                    role.to_owned(),
                    declared_adapter_for_role(&parsed, role).unwrap(),
                )
            })
            .collect(),
        tools_toml_path: Some(base.join("credentials/tools.toml")),
        ..Default::default()
    };
    let _credential =
        kio_core::test_control::TestEnvGuard::set("KIO_TEST_RECOVERY_ROTATING_KEY", "before");
    let _workspace =
        kio_core::test_control::TestEnvGuard::set("KIO_MISTRAL_WORKSPACE_ID", "unchanged");
    let _project = kio_core::test_control::TestEnvGuard::set("KIO_GEMINI_PROJECT_ID", "unchanged");
    let capture = |context: &BatchRecoveryContext| {
        (
            EnvMistralBatchClient::new(context)
                .unwrap()
                .provider_scope_id()
                .unwrap(),
            EnvGeminiBatchClient::new(context)
                .unwrap()
                .provider_scope_id()
                .unwrap(),
        )
    };
    let (before, credential_rotated, device_rotated) = with_runtime_settings(settings, || {
        let original = BatchRecoveryContext::from_key([7; 32]);
        let before = capture(&original);
        let device_rotated = capture(&BatchRecoveryContext::from_key([8; 32]));
        let _rotated =
            kio_core::test_control::TestEnvGuard::set("KIO_TEST_RECOVERY_ROTATING_KEY", "after");
        (before, capture(&original), device_rotated)
    });
    for current in [credential_rotated, device_rotated] {
        let ledger_dir = canonical_tempdir();
        let ledger = LedgerDb::initialize(ledger_dir.path().join("ledger.sqlite")).unwrap();
        let row = reserved_row(&ledger, "markdownize", "profile", Some(&before.0));
        let mut mistral = mistral_client(&row);
        mistral.scope = Some(current.0);
        assert!(poll_attributed_markdownize_batch_job(&mistral, &row).is_none());
        cleanup_batch_residue(
            &ledger,
            &mistral,
            &row.key,
            row.intent_token.as_deref().unwrap(),
            Some("upload-recorded"),
            row.provider_scope_id.as_deref(),
        )
        .unwrap();
        assert_eq!(
            (
                mistral.polls.get(),
                mistral.fetches.get(),
                mistral.deletes.get()
            ),
            (0, 0, 0)
        );
        assert_eq!(get_batch_request(&ledger, &row.key).unwrap().unwrap(), row);
        let row = reserved_row(&ledger, EMBEDDING_ADAPTER_KIND, "profile", Some(&before.1));
        let gemini = GeminiClient {
            scope: Some(current.1),
            record: GeminiBatchJobRecord {
                name: "batches/recorded".into(),
                display_name: batch_display_name(row.intent_token.as_deref().unwrap()),
                state: GeminiBatchState::Succeeded,
                responses_file: None,
            },
            polls: Cell::new(0),
        };
        assert!(poll_attributed_embedding_batch_job(&gemini, &row, Some("profile")).is_none());
        assert_eq!(gemini.polls.get(), 0);
        assert_eq!(get_batch_request(&ledger, &row.key).unwrap().unwrap(), row);
    }
}

#[test]
fn reconcile_json_and_inventory_debug_do_not_expose_recovery_scope() {
    let _guard = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let root = canonical_tempdir();
    let _data = kio_core::test_control::TestEnvGuard::set("XDG_DATA_HOME", root.path());
    let ledger = LedgerDb::initialize(root.path().join("device/ledger.sqlite")).unwrap();
    let inventory = kio_adapter::batch_inventory::ProviderInventory {
        provider_scope_id: "kio-batch-recovery:v1:mistral:private-hmac".into(),
        jobs: vec![kio_adapter::batch_inventory::ProviderJobRecord {
            job_id: "foreign-job".into(),
            intent_token: None,
            task_key: None,
        }],
        uploads: vec![kio_adapter::batch_inventory::ProviderUploadRecord {
            upload_id: "foreign-upload".into(),
            filename_token: None,
        }],
    };
    assert!(!format!("{inventory:?}").contains("private-hmac"));
    let report = run_orphan_attribution_walk(&ledger, &[inventory]).unwrap();
    assert_eq!(report.unknown.len(), 1);
    assert_eq!(report.unknown_uploads.len(), 1);
    let output =
        json!({ "unknown": report.unknown, "unknown_uploads": report.unknown_uploads }).to_string();
    assert!(!output.contains("provider_scope_id"));
    assert!(!output.contains("private-hmac"));
}
