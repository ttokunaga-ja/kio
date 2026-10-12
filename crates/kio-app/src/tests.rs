use kio_core::control::ControlHandle;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::sync::Arc;

use crate::context::{AppContext, Interaction};

struct TestInteraction;

impl Interaction for TestInteraction {
    fn confirm(&self, _: &str) -> kio_core::Result<bool> {
        Ok(false)
    }
    fn read_input(&self, _: usize) -> kio_core::Result<Vec<u8>> {
        Ok(Vec::new())
    }
    fn is_interactive(&self) -> bool {
        false
    }
    fn diagnostic(&self, _: &str) {}
    fn write_diagnostic(&self, _: &str) -> kio_core::Result<()> {
        Ok(())
    }
    fn flush_diagnostics(&self) -> kio_core::Result<()> {
        Ok(())
    }
}

fn test_context(working_directory: &Path) -> AppContext {
    AppContext {
        working_directory: working_directory.to_path_buf(),
        interaction: Arc::new(TestInteraction),
    }
}

#[cfg(debug_assertions)]
#[test]
fn recognized_mistral_ocr_test_control_allows_sync_admission_without_a_key() {
    use crate::markdownize::batch::markdownize_test_credential_available;
    use kio_core::test_control::DebugTestControl;
    use kio_core::test_control::{AdapterTestControl, MistralOcrMode, Selector};

    let recognized = DebugTestControl {
        adapters: AdapterTestControl {
            mistral_ocr: Selector::Known(MistralOcrMode::Mock),
            ..Default::default()
        },
        ..Default::default()
    };
    assert!(markdownize_test_credential_available(&recognized));

    let unrecognized = DebugTestControl {
        adapters: AdapterTestControl {
            mistral_ocr: Selector::Unknown("unrecognized".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    assert!(!markdownize_test_credential_available(&unrecognized));
}

#[test]
fn test_context_binds_the_requested_working_directory() {
    let directory = tempfile::tempdir().unwrap();
    let context = test_context(directory.path());
    assert_eq!(context.working_directory, directory.path());
    assert!(!context.interaction.is_interactive());
}

#[test]
fn index_reconciles_more_than_legacy_child_cap_and_deep_pages_in_one_run() {
    use crate::commands::IndexArgs;
    use crate::management::{ExplicitRoot, initialize_explicit_root};

    let _environment_lock = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    #[cfg(unix)]
    {
        const CHILD: &str = "KIO_CHILD_DISCOVERY_FD_LIMIT_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            use std::os::unix::process::CommandExt;
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "tests::index_reconciles_more_than_legacy_child_cap_and_deep_pages_in_one_run",
                    "--nocapture",
                ])
                .env(CHILD, "1");
            // Only the isolated fixture child gets the ordinary Linux
            // descriptor budget; the parallel test runner is unchanged.
            unsafe {
                command.pre_exec(|| {
                    let mut limit = std::mem::zeroed::<libc::rlimit>();
                    if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    limit.rlim_cur = 1024.min(limit.rlim_max);
                    if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "descriptor-limited discovery failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            return;
        }
    }
    let temporary = tempfile::tempdir().unwrap();
    let base = temporary.path().canonicalize().unwrap();
    let device = base.join("device");
    fs::create_dir(&device).unwrap();
    // Keep device-global SQLite and registry state outside the managed
    // root, and restore these process variables under the shared lock.
    let _home = kio_core::test_control::TestEnvGuard::set("HOME", &device);
    let _config =
        kio_core::test_control::TestEnvGuard::set("XDG_CONFIG_HOME", device.join("config"));
    let _data = kio_core::test_control::TestEnvGuard::set("XDG_DATA_HOME", &device);
    let _cache = kio_core::test_control::TestEnvGuard::set("XDG_CACHE_HOME", device.join("cache"));
    let root = base.join("scope");
    fs::create_dir(&root).unwrap();
    match initialize_explicit_root(&root, &kio_core::control::ControlHandle::default()).unwrap() {
        ExplicitRoot::Created(_) => {}
        ExplicitRoot::Existing(_) => panic!("new root must be created"),
    }
    for index in 0..513 {
        fs::create_dir(root.join(format!("flat-{index:04}"))).unwrap();
    }
    let mut deep = root.join("deep");
    fs::create_dir(&deep).unwrap();
    let mut deep_scopes = vec![deep.clone()];
    for index in 0..64 {
        deep = deep.join(format!("d-{index:02}"));
        fs::create_dir(&deep).unwrap();
        deep_scopes.push(deep.clone());
    }

    let output = crate::execute(
        &test_context(&root),
        crate::commands::CommandRequest::Index(IndexArgs {
            preview: false,
            yes: false,
            online: false,
            offline: true,
            realtime: false,
            batch: false,
        }),
    )
    .unwrap();

    let discovery = output["child_scope_discovery"].as_object().unwrap();
    assert_eq!(discovery["has_more"], false);
    assert_eq!(discovery["rows_total"], 578);
    assert_eq!(discovery["rows_omitted"], 450);
    assert!((0..513).all(|index| {
        root.join(format!("flat-{index:04}/.kio/scope.json"))
            .is_file()
    }));
    assert!(
        deep_scopes
            .iter()
            .all(|path| path.join(".kio/scope.json").is_file())
    );
    assert!(
        !root
            .join(".kio/internal/child-discovery/frontier.json")
            .exists()
    );
}

#[test]
fn retry_reset_confirmation_and_lane_contract_is_enforced_for_direct_callers() {
    use crate::commands::{BatchArgs, BatchCommand, RetryArgs};

    let retry = |reset_violations: Option<&str>, yes, online| BatchArgs {
        command: Some(BatchCommand::Retry(RetryArgs {
            reset_violations: reset_violations.map(str::to_owned),
            resend_unknown: None,
            yes,
            online,
            offline: false,
            realtime: false,
            batch: false,
        })),
    };

    assert!(crate::batch::command::validate_batch_args(&retry(None, true, false)).is_err());
    assert!(
        crate::batch::command::validate_batch_args(&retry(Some("token"), false, true)).is_err()
    );
    assert!(crate::batch::command::validate_batch_args(&retry(Some("token"), true, false)).is_ok());

    let resend = |selector: Option<&str>, yes, realtime| BatchArgs {
        command: Some(BatchCommand::Retry(RetryArgs {
            reset_violations: None,
            resend_unknown: selector.map(str::to_owned),
            yes,
            online: false,
            offline: false,
            realtime,
            batch: false,
        })),
    };
    assert!(
        crate::batch::command::validate_batch_args(&resend(Some("token"), false, false)).is_err()
    );
    assert!(
        crate::batch::command::validate_batch_args(&resend(Some("token"), true, true)).is_err()
    );
    assert!(
        crate::batch::command::validate_batch_args(&resend(Some("token"), true, false)).is_ok()
    );
}

#[test]
fn materialized_source_image_cas_failure_is_retryable_not_contract_reject() {
    use kio_adapter::types::{
        MarkdownUnit, MarkdownizeMode as AdapterMarkdownizeMode, MarkdownizeResponse, UnitKind,
    };
    use kio_pipeline::prepare::{PreparedUnit, UnitFingerprint, UnitType, hash_bytes};
    use kio_pipeline::task::{TaskDescriptor, TaskStatus, TaskType};

    const PNG: &[u8] = include_bytes!(
        "../../kio-eval/acceptance-fixtures/v1/authenticated-local-public/image.png"
    );
    let root = tempfile::tempdir().unwrap();
    let repo = Repository::init(root.path()).unwrap();
    // `ObjectStore` resolves image storage lazily. This makes the post-response
    // CAS publication fail after source bytes have passed bounded validation.
    fs::write(repo.kio_dir().join("objects/image"), b"not a directory").unwrap();
    let raw_hash = hash_bytes(PNG);
    let profile_hash = hash_bytes(b"online profile");
    let prepared = PreparedUnit {
        order: 0,
        unit_key: "image:0".to_owned(),
        unit_type: UnitType::Image,
        prepared_hash: raw_hash.clone(),
        preparation_profile_hash: hash_bytes(b"prepare profile"),
        fingerprint: UnitFingerprint {
            perceptual_hash: hash_bytes(b"perceptual"),
            text_hash: hash_bytes(b"text"),
            visual_hash: hash_bytes(b"visual"),
        },
        mime: Some("image/png".to_owned()),
        page_number: None,
    };
    let task = TaskDescriptor {
        task_id: "task_known_provider_result".to_owned(),
        task_type: TaskType::Markdownize,
        mode: Some(kio_pipeline::markdownize::MarkdownizeMode::Full),
        input_path: "image.png".to_owned(),
        input_hash: raw_hash.clone(),
        previous_raw_hash: None,
        parent_run_id: None,
        changed_unit_keys: Vec::new(),
        output_ref: "online:mistral_ocr_markdownize".to_owned(),
        unit_keys: None,
        status: TaskStatus::Running,
        attempts: 1,
        next_retry_at: None,
        deadline: None,
        heartbeat_at: None,
        fallback_reason: None,
        created_at: "2026-09-08T00:00:00Z".to_owned(),
        bbox_annotation_enabled: None,
        hold_reason: None,
        reserved_usd: None,
        reserved_month: None,
        reservation_id: None,
    };
    let response = MarkdownizeResponse {
        mode_used: AdapterMarkdownizeMode::Full,
        updated_units: vec![MarkdownUnit {
            unit_key: prepared.unit_key.clone(),
            unit_type: UnitKind::Image,
            markdown: "recognized OCR text\n".to_owned(),
            owned_image_hashes: BTreeSet::new(),
            metadata: BTreeMap::new(),
        }],
        unchanged_unit_keys: Vec::new(),
        added_units: Vec::new(),
        removed_unit_keys: Vec::new(),
        failed_units: Vec::new(),
        fallback_to_full: false,
        reason: None,
        usage: None,
    };

    let failure = crate::markdownize::execution::materialize_online_markdownize_response(
        &repo,
        &task,
        vec![prepared],
        "image/png",
        PNG,
        None,
        profile_hash,
        response,
        &[],
    )
    .unwrap_err();
    assert_eq!(
        failure.retry_kind,
        kio_pipeline::task::RetryErrorKind::NetworkError
    );
}

#[test]
fn invalid_online_markdown_is_rejected_before_normalized_publication() {
    use kio_adapter::types::{
        MarkdownUnit, MarkdownizeMode as AdapterMarkdownizeMode, MarkdownizeResponse, UnitKind,
    };
    use kio_pipeline::prepare::{PreparedUnit, UnitFingerprint, UnitType, hash_bytes};
    use kio_pipeline::task::{RetryErrorKind, TaskDescriptor, TaskStatus, TaskType};

    const PNG: &[u8] = include_bytes!(
        "../../kio-eval/acceptance-fixtures/v1/authenticated-local-public/image.png"
    );
    let root = tempfile::tempdir().unwrap();
    let repo = Repository::init(root.path()).unwrap();
    let raw_hash = hash_bytes(PNG);
    let profile_hash = hash_bytes(b"online profile");
    let prepared = PreparedUnit {
        order: 0,
        unit_key: "image:0".to_owned(),
        unit_type: UnitType::Image,
        prepared_hash: raw_hash.clone(),
        preparation_profile_hash: hash_bytes(b"prepare profile"),
        fingerprint: UnitFingerprint {
            perceptual_hash: hash_bytes(b"perceptual"),
            text_hash: hash_bytes(b"text"),
            visual_hash: hash_bytes(b"visual"),
        },
        mime: Some("image/png".to_owned()),
        page_number: None,
    };
    let task = TaskDescriptor {
        task_id: "task_invalid_known_provider_result".to_owned(),
        task_type: TaskType::Markdownize,
        mode: Some(kio_pipeline::markdownize::MarkdownizeMode::Full),
        input_path: "image.png".to_owned(),
        input_hash: raw_hash.clone(),
        previous_raw_hash: None,
        parent_run_id: None,
        changed_unit_keys: Vec::new(),
        output_ref: "online:mistral_ocr_markdownize".to_owned(),
        unit_keys: None,
        status: TaskStatus::Running,
        attempts: 1,
        next_retry_at: None,
        deadline: None,
        heartbeat_at: None,
        fallback_reason: None,
        created_at: "2026-09-08T00:00:00Z".to_owned(),
        bbox_annotation_enabled: None,
        hold_reason: None,
        reserved_usd: None,
        reserved_month: None,
        reservation_id: None,
    };
    let mut response = MarkdownizeResponse {
        mode_used: AdapterMarkdownizeMode::Full,
        updated_units: vec![MarkdownUnit {
            unit_key: prepared.unit_key.clone(),
            unit_type: UnitKind::Image,
            markdown: "<script>untrusted provider markup</script>\n".to_owned(),
            owned_image_hashes: BTreeSet::new(),
            metadata: BTreeMap::new(),
        }],
        unchanged_unit_keys: Vec::new(),
        added_units: Vec::new(),
        removed_unit_keys: Vec::new(),
        failed_units: Vec::new(),
        fallback_to_full: false,
        reason: None,
        usage: None,
    };

    let failure = crate::markdownize::execution::materialize_online_markdownize_response(
        &repo,
        &task,
        vec![prepared],
        "image/png",
        PNG,
        None,
        profile_hash.clone(),
        response.clone(),
        &[],
    )
    .unwrap_err();
    assert_eq!(failure.retry_kind, RetryErrorKind::ContractViolation);
    response.updated_units[0].unit_key = "image:unknown".to_owned();
    response.updated_units[0].markdown = "otherwise valid text\n".to_owned();
    let unknown_unit = crate::markdownize::execution::materialize_online_markdownize_response(
        &repo,
        &task,
        vec![PreparedUnit {
            order: 0,
            unit_key: "image:0".to_owned(),
            unit_type: UnitType::Image,
            prepared_hash: raw_hash.clone(),
            preparation_profile_hash: hash_bytes(b"prepare profile"),
            fingerprint: UnitFingerprint {
                perceptual_hash: hash_bytes(b"perceptual"),
                text_hash: hash_bytes(b"text"),
                visual_hash: hash_bytes(b"visual"),
            },
            mime: Some("image/png".to_owned()),
            page_number: None,
        }],
        "image/png",
        PNG,
        None,
        profile_hash.clone(),
        response,
        &[],
    )
    .unwrap_err();
    assert_eq!(unknown_unit.retry_kind, RetryErrorKind::ContractViolation);
    assert!(
        !kio_pipeline::markdownize::normalized_instance_dir(
            repo.kio_dir(),
            &raw_hash,
            &profile_hash,
            0,
        )
        .exists()
    );
}

#[test]
fn valid_partial_online_response_is_materialized() {
    use kio_adapter::types::{
        FailedUnit, MarkdownUnit, MarkdownizeMode as AdapterMarkdownizeMode, MarkdownizeResponse,
        UnitKind,
    };
    use kio_pipeline::prepare::{PreparedUnit, UnitFingerprint, UnitType, hash_bytes};
    use kio_pipeline::task::{TaskDescriptor, TaskStatus, TaskType};

    let root = tempfile::tempdir().unwrap();
    let repo = Repository::init(root.path()).unwrap();
    let raw = b"already prepared test PDF pages";
    let raw_hash = hash_bytes(raw);
    let profile_hash = hash_bytes(b"online profile");
    let page = |order, key: &str| PreparedUnit {
        order,
        unit_key: key.to_owned(),
        unit_type: UnitType::Page,
        prepared_hash: hash_bytes(key.as_bytes()),
        preparation_profile_hash: hash_bytes(b"prepare profile"),
        fingerprint: UnitFingerprint {
            perceptual_hash: hash_bytes(b"perceptual"),
            text_hash: hash_bytes(key.as_bytes()),
            visual_hash: hash_bytes(b"visual"),
        },
        mime: Some("application/pdf".to_owned()),
        page_number: Some(order + 1),
    };
    let prepared = vec![page(0, "page:1"), page(1, "page:2")];
    let task = TaskDescriptor {
        task_id: "task_valid_partial".to_owned(),
        task_type: TaskType::Markdownize,
        mode: Some(kio_pipeline::markdownize::MarkdownizeMode::Full),
        input_path: "document.pdf".to_owned(),
        input_hash: raw_hash.clone(),
        previous_raw_hash: None,
        parent_run_id: None,
        changed_unit_keys: Vec::new(),
        output_ref: "online:mistral_ocr_markdownize".to_owned(),
        unit_keys: None,
        status: TaskStatus::Running,
        attempts: 1,
        next_retry_at: None,
        deadline: None,
        heartbeat_at: None,
        fallback_reason: None,
        created_at: "2026-09-08T00:00:00Z".to_owned(),
        bbox_annotation_enabled: None,
        hold_reason: None,
        reserved_usd: None,
        reserved_month: None,
        reservation_id: None,
    };
    let response = MarkdownizeResponse {
        mode_used: AdapterMarkdownizeMode::Full,
        updated_units: vec![MarkdownUnit {
            unit_key: "page:1".to_owned(),
            unit_type: UnitKind::Page,
            markdown: "accepted first page\n".to_owned(),
            owned_image_hashes: BTreeSet::new(),
            metadata: BTreeMap::new(),
        }],
        unchanged_unit_keys: Vec::new(),
        added_units: Vec::new(),
        removed_unit_keys: Vec::new(),
        failed_units: vec![FailedUnit {
            unit_key: "page:2".to_owned(),
            error_kind: "network_error".to_owned(),
        }],
        fallback_to_full: false,
        reason: None,
        usage: None,
    };
    let outcome = crate::markdownize::execution::materialize_online_markdownize_response(
        &repo,
        &task,
        prepared.clone(),
        "application/pdf",
        raw,
        None,
        profile_hash.clone(),
        response,
        &[],
    )
    .unwrap();
    assert_eq!(outcome.status, TaskStatus::Partial);
    let retry = MarkdownizeResponse {
        mode_used: AdapterMarkdownizeMode::Full,
        updated_units: vec![MarkdownUnit {
            unit_key: "page:2".to_owned(),
            unit_type: UnitKind::Page,
            markdown: "accepted second page\n".to_owned(),
            owned_image_hashes: BTreeSet::new(),
            metadata: BTreeMap::new(),
        }],
        unchanged_unit_keys: Vec::new(),
        added_units: Vec::new(),
        removed_unit_keys: Vec::new(),
        failed_units: Vec::new(),
        fallback_to_full: false,
        reason: None,
        usage: None,
    };
    let retry_keys = BTreeSet::from(["page:2".to_owned()]);
    let retried = crate::markdownize::execution::materialize_online_markdownize_response(
        &repo,
        &task,
        prepared,
        "application/pdf",
        raw,
        Some(&retry_keys),
        profile_hash.clone(),
        retry,
        &[],
    )
    .unwrap();
    assert_eq!(retried.status, TaskStatus::Done);
    let retained = kio_pipeline::markdownize::load_validated_normalized_instance(
        repo.kio_dir(),
        &raw_hash,
        &profile_hash,
        0,
    )
    .unwrap();
    assert_eq!(retained.units.len(), 2);
    assert_eq!(retained.units[0].markdown, "accepted first page\n");
    assert_eq!(retained.units[1].markdown, "accepted second page\n");
    assert!(
        kio_pipeline::markdownize::normalized_instance_dir(
            repo.kio_dir(),
            &raw_hash,
            &profile_hash,
            0,
        )
        .exists()
    );

    let all_failed_profile = hash_bytes(b"online profile all failed");
    let all_failed = MarkdownizeResponse {
        mode_used: AdapterMarkdownizeMode::Full,
        updated_units: Vec::new(),
        unchanged_unit_keys: Vec::new(),
        added_units: Vec::new(),
        removed_unit_keys: Vec::new(),
        failed_units: vec![
            FailedUnit {
                unit_key: "page:1".to_owned(),
                error_kind: "network_error".to_owned(),
            },
            FailedUnit {
                unit_key: "page:2".to_owned(),
                error_kind: "network_error".to_owned(),
            },
        ],
        fallback_to_full: false,
        reason: None,
        usage: None,
    };
    let all_failed_outcome =
        crate::markdownize::execution::materialize_online_markdownize_response(
            &repo,
            &task,
            vec![page(0, "page:1"), page(1, "page:2")],
            "application/pdf",
            raw,
            None,
            all_failed_profile.clone(),
            all_failed,
            &[],
        )
        .unwrap();
    assert_eq!(all_failed_outcome.status, TaskStatus::Failed);
    let all_failed_instance = kio_pipeline::markdownize::load_validated_normalized_instance(
        repo.kio_dir(),
        &raw_hash,
        &all_failed_profile,
        0,
    )
    .unwrap();
    assert!(all_failed_instance.units.is_empty());
    assert!(all_failed_instance.manifest.units.iter().all(|unit| {
        unit.status == kio_pipeline::markdownize::UnitStatus::Failed
            && unit.error_kind.as_deref() == Some("network_error")
    }));
}

#[test]
fn unchanged_image_unit_reuses_pinned_markdown_verbatim() {
    use kio_adapter::types::{MarkdownizeMode as AdapterMarkdownizeMode, MarkdownizeResponse};
    use kio_pipeline::markdownize::{
        MarkdownizeMode, NormalizedInstanceManifest, NormalizedUnitObject,
    };
    use kio_pipeline::prepare::{PreparedUnit, UnitFingerprint, UnitType, hash_bytes};

    let root = tempfile::tempdir().unwrap();
    let repo = Repository::init(root.path()).unwrap();
    // Deliberately not image bytes: if an unchanged unit ever enters the
    // fresh-output source-retention path, this test fails before it can
    // mutate the pinned Markdown or write an Image CAS object.
    let raw_bytes = b"not a new image payload";
    let raw_hash = hash_bytes(raw_bytes);
    let profile_hash = hash_bytes(b"profile");
    let prepared = PreparedUnit {
        order: 0,
        unit_key: "image:0".to_owned(),
        unit_type: UnitType::Image,
        prepared_hash: raw_hash.clone(),
        preparation_profile_hash: hash_bytes(b"prepare profile"),
        fingerprint: UnitFingerprint {
            perceptual_hash: hash_bytes(b"perceptual"),
            text_hash: hash_bytes(b"text"),
            visual_hash: hash_bytes(b"visual"),
        },
        mime: Some("image/png".to_owned()),
        page_number: None,
    };
    let previous_markdown = "legacy pinned OCR text\n".to_owned();
    let previous = crate::markdownize::execution::PreviousInstance {
        manifest: NormalizedInstanceManifest {
            raw_hash: raw_hash.clone(),
            tool_profile_hash: profile_hash.clone(),
            r#gen: 0,
            parent_gen: None,
            run_id: "run_previous".to_owned(),
            units: Vec::new(),
            generated_at: "2026-09-08T00:00:00Z".to_owned(),
        },
        units: vec![NormalizedUnitObject {
            unit_key: prepared.unit_key.clone(),
            unit_type: UnitType::Image,
            raw_hash: raw_hash.clone(),
            prepared_hash: raw_hash.clone(),
            preparation_profile_hash: prepared.preparation_profile_hash.clone(),
            tool_profile_hash: profile_hash.clone(),
            r#gen: 0,
            mode: MarkdownizeMode::Full,
            markdown: previous_markdown.clone(),
            owned_image_hashes: BTreeSet::new(),
            metadata: BTreeMap::new(),
            reused_from: None,
            generated_at: "2026-09-08T00:00:00Z".to_owned(),
        }],
        prepared_units: vec![prepared.clone()],
    };
    let response = MarkdownizeResponse {
        mode_used: AdapterMarkdownizeMode::Incremental,
        updated_units: Vec::new(),
        unchanged_unit_keys: vec![prepared.unit_key.clone()],
        added_units: Vec::new(),
        removed_unit_keys: Vec::new(),
        failed_units: Vec::new(),
        fallback_to_full: false,
        reason: None,
        usage: None,
    };

    let units = crate::markdownize::execution::normalized_units_from_response(
        &repo,
        "image/png",
        raw_bytes,
        &response,
        &[prepared],
        Some(&previous),
        &raw_hash,
        &profile_hash,
        0,
        MarkdownizeMode::Incremental,
        "2026-09-08T00:00:01Z",
    )
    .unwrap();
    assert_eq!(units.len(), 1);
    assert_eq!(units[0].markdown, previous_markdown);
    assert!(
        repo.object_store()
            .inspect_content_object(kio_core::cas::ContentObjectKind::Image, &raw_hash)
            .is_err()
    );
}

#[test]
fn preparation_profile_drift_bumps_generation_despite_changed_unit_keys() {
    use kio_pipeline::markdownize::{
        MarkdownizeMode, NormalizedUnitObject, persist_normalized_instance,
    };
    use kio_pipeline::prepare::{PreparedUnit, UnitFingerprint, UnitType, hash_bytes};
    use kio_pipeline::task::RetryErrorKind;

    let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    Repository::init(root.path()).unwrap();
    let kio_dir = root.path().join(".kio");
    std::fs::create_dir_all(&kio_dir).unwrap();
    let raw_hash = hash_bytes(b"unchanged raw bytes");
    let markdown_profile_hash = hash_bytes(b"markdown profile");
    let old_preparation_profile = hash_bytes(b"old renderer profile");
    let new_preparation_profile = hash_bytes(b"new renderer profile");
    let old_prepared = PreparedUnit {
        order: 0,
        unit_key: "page:1".to_owned(),
        unit_type: UnitType::Page,
        prepared_hash: hash_bytes(b"same page bytes"),
        preparation_profile_hash: old_preparation_profile.clone(),
        fingerprint: UnitFingerprint {
            perceptual_hash: hash_bytes(b"perceptual"),
            text_hash: hash_bytes(b"text"),
            visual_hash: hash_bytes(b"visual"),
        },
        mime: Some("application/pdf".to_owned()),
        page_number: Some(1),
    };
    let old_unit = NormalizedUnitObject {
        unit_key: old_prepared.unit_key.clone(),
        unit_type: old_prepared.unit_type,
        raw_hash: raw_hash.clone(),
        prepared_hash: old_prepared.prepared_hash.clone(),
        preparation_profile_hash: old_preparation_profile,
        tool_profile_hash: markdown_profile_hash.clone(),
        r#gen: 0,
        mode: MarkdownizeMode::Full,
        markdown: "old normalized text\n".to_owned(),
        owned_image_hashes: BTreeSet::new(),
        metadata: BTreeMap::new(),
        reused_from: None,
        generated_at: "2026-09-08T00:00:00Z".to_owned(),
    };
    let manifest = crate::markdownize::execution::manifest_from_units(
        std::slice::from_ref(&old_prepared),
        &[old_unit],
        &raw_hash,
        &markdown_profile_hash,
        0,
        None,
        "run_profile_drift",
        "2026-09-08T00:00:00Z",
        RetryErrorKind::ContractViolation,
    );
    let unit = NormalizedUnitObject {
        unit_key: "page:1".to_owned(),
        unit_type: UnitType::Page,
        raw_hash: raw_hash.clone(),
        prepared_hash: hash_bytes(b"same page bytes"),
        preparation_profile_hash: hash_bytes(b"old renderer profile"),
        tool_profile_hash: markdown_profile_hash.clone(),
        r#gen: 0,
        mode: MarkdownizeMode::Full,
        markdown: "old normalized text\n".to_owned(),
        owned_image_hashes: BTreeSet::new(),
        metadata: BTreeMap::new(),
        reused_from: None,
        generated_at: "2026-09-08T00:00:00Z".to_owned(),
    };
    persist_normalized_instance(&kio_dir, &manifest, &[unit]).unwrap();

    let fresh = PreparedUnit {
        unit_key: "page:2".to_owned(),
        preparation_profile_hash: new_preparation_profile,
        page_number: Some(2),
        ..old_prepared
    };
    assert_eq!(
        crate::index::pipeline::prepared_hash_drift_new_gen(
            &kio_dir,
            &raw_hash,
            &markdown_profile_hash,
            &[fresh],
            &ControlHandle::default()
        ),
        Some(1)
    );
}

#[cfg(unix)]
use std::os::unix::fs::symlink;

use kio_adapter::catalog::deterministic_embedding_vector;
use kio_index::ChunkRow;

use crate::embedding::transport::LaneOverride;
use crate::embedding::transport::effective_invocation_lane;
use crate::embedding::transport::embedding_usd_per_token;
use crate::embedding::transport::estimate_embedding_cost;
use crate::embedding::transport::estimate_embedding_tokens;
use crate::embedding::transport::lane_rate;
use crate::embedding::transport::query_embedding_send_lane;
use crate::embedding::transport::realtime_lane_requested;
use crate::embedding::transport::resolve_invocation_lane;
use crate::index::chunk_ledger::ChunkPublicationEvent;
use crate::index::chunk_ledger::StoredChunk;
use crate::index::chunk_ledger::append_chunk_publication_events;
use crate::index::chunk_ledger::append_stored_chunks;
use crate::index::chunk_ledger::parse_chunk_ledger;
use crate::index::chunk_ledger::read_chunk_publication_events;
use crate::index::chunk_ledger::read_stored_chunks;
#[cfg(unix)]
use crate::index::chunk_ledger::replace_chunk_ledger_contents;
#[cfg(unix)]
use crate::index::chunk_ledger::truncate_torn_chunk_tail;
use crate::index::rebuild::AssociationAppend;
use crate::index::rebuild::append_new_chunk_association;
use crate::index::rebuild::unit_authorities_from_inputs;
use crate::markdownize::batch::MarkdownizeSendLane;
use crate::markdownize::batch::markdownize_send_lane;
use crate::search::projection::publication_is_compatible;
use crate::search::projection::publication_is_include_deleted_compatible;
use crate::search::projection::publication_ranks;
use crate::search::projection::write_through_projection_with_requested_at;
use kio_adapter::traits::PreferredRequestKind;
use kio_core::scope::Repository;
use kio_index::chunking::NormalizedUnitInput;

#[test]
fn embedding_tool_lock_labels_cover_every_execution_mode() {
    use kio_adapter::types::ExecutionMode;

    assert_eq!(
        crate::embedding::transport::embedding_execution_labels(ExecutionMode::OnlineApi),
        ("online_api", "online")
    );
    assert_eq!(
        crate::embedding::transport::embedding_execution_labels(ExecutionMode::OfflineApi),
        ("offline_api", "offline")
    );
    assert_eq!(
        crate::embedding::transport::embedding_execution_labels(
            ExecutionMode::DeterministicLibrary
        ),
        ("deterministic_library", "deterministic")
    );
}

#[test]
fn offline_keeps_only_the_in_process_embedding_evaluator_eligible() {
    use kio_adapter::types::ExecutionMode;

    assert!(
        crate::embedding::transport::embedding_mode_transport_allowed(
            ExecutionMode::DeterministicLibrary,
            true,
        )
    );
    assert!(
        !crate::embedding::transport::embedding_mode_transport_allowed(
            ExecutionMode::OfflineApi,
            true,
        )
    );
    assert!(
        !crate::embedding::transport::embedding_mode_transport_allowed(
            ExecutionMode::OnlineApi,
            true,
        )
    );
}

#[test]
fn embedding_cas_failure_cannot_publish_source_or_replica_vectors() {
    if !super::contextual_embedding_tests::isolated_child_for(
        "tests::embedding_cas_failure_cannot_publish_source_or_replica_vectors",
    ) {
        return;
    }
    use crate::embedding::enrichment::EmbeddingProjection;
    use crate::embedding::enrichment::EmbeddingSendGroup;
    use crate::embedding::enrichment::bind_embedding_work;
    use crate::embedding::enrichment::persist_group_vector;
    use crate::embedding::transport::chunk_embedding_hash;
    use crate::embedding_owners::RetainedEmbeddingChunk;
    use crate::search::scope::current_embedding_policy;
    use kio_adapter::catalog::DeclaredEmbeddingProfile;
    use kio_core::cas::ObjectStore;
    use kio_index::embedding_store;
    use kio_index::fts::FtsSchemaConfig;
    use kio_index::fts::FtsTokenizer;
    use kio_index::fts::SqliteFtsIndex;

    for corrupt_cas in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("scope");
        fs::create_dir(&root).unwrap();
        let repo = match crate::management::initialize_explicit_root(
            &root,
            &kio_core::control::ControlHandle::default(),
        )
        .unwrap()
        {
            crate::management::ExplicitRoot::Created(repo) => repo,
            crate::management::ExplicitRoot::Existing(_) => panic!("fresh root"),
        };
        let mut index = SqliteFtsIndex::open(
            temp.path().join("projection.sqlite"),
            FtsSchemaConfig {
                tokenizer: FtsTokenizer::Trigram,
            },
        )
        .unwrap();
        let row = ledger_test_chunk().row;
        index.index_chunk(&row).unwrap();
        let chunk = RetainedEmbeddingChunk {
            chunk_id: row.chunk_id,
            text: row.text.into(),
            text_hash: row.text_hash,
            raw_path: row.raw_path,
            requires_secret_approval: false,
            is_head_owner: false,
        };
        let profile = DeclaredEmbeddingProfile {
            tool_id: "test_embedding".into(),
            dimensions: 768,
            distance: "cosine".into(),
            modality: "multimodal".into(),
            profile_hash: kio_core::cas::hash_bytes(b"embedding profile"),
        };
        let chunk = bind_embedding_work(vec![chunk], &profile)
            .unwrap()
            .remove(0);
        let group = EmbeddingSendGroup {
            embedding_hash: chunk_embedding_hash(&chunk, &profile).unwrap(),
            representative: &chunk,
            members: vec![&chunk],
        };
        let mut vector = vec![0.0; 768];
        vector[0] = 1.0;
        let object_path = ObjectStore::new(repo.kio_dir())
            .embedding_path(&group.embedding_hash)
            .unwrap();
        if corrupt_cas {
            fs::create_dir_all(object_path.parent().unwrap()).unwrap();
            fs::write(&object_path, b"corrupt immutable object").unwrap();
        }
        let mut replica = kio_index::aggregator::ScopeDelta::default();
        let policy = current_embedding_policy(&repo).unwrap();
        let contexts = BTreeMap::from([(
            chunk.chunk_id.clone(),
            embedding_store::chunk_embedding_context(&chunk.raw_path),
        )]);
        let projection = EmbeddingProjection {
            repo: &repo,
            policy: &policy,
            selected_contexts: &contexts,
            invalidated: std::cell::Cell::new(false),
        };
        let result = persist_group_vector(
            index.connection(),
            &profile,
            &group,
            &vector,
            &BTreeSet::new(),
            &projection,
            &mut replica,
        );
        assert_eq!(result.is_ok(), !corrupt_cas);
        for table in ["embeddings", "chunk_vec"] {
            let count: u64 = index
                .connection()
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, u64::from(!corrupt_cas), "{table}");
        }
        assert_eq!(replica.vectors_added.len(), usize::from(!corrupt_cas));
        if corrupt_cas {
            assert_eq!(fs::read(object_path).unwrap(), b"corrupt immutable object");
        } else {
            assert_eq!(
                repo.object_store()
                    .read_embedding(&group.embedding_hash)
                    .unwrap()
                    .vector,
                vector
            );
        }
    }
}

fn ledger_test_chunk() -> StoredChunk {
    let mut chunk = StoredChunk {
        rowid: 1,
        association_rowid: 1,
        row: ChunkRow {
            chunk_id: String::new(),
            raw_hash: "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                .into(),
            tool_profile_hash:
                "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc".into(),
            r#gen: 1,
            unit_key: "unit".into(),
            unit_content_hash:
                "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd".into(),
            chunking_config_hash:
                "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".into(),
            raw_path: "input.md".into(),
            heading_path: None,
            section_id: None,
            byte_start: 0,
            byte_end: 1,
            text_hash: kio_core::cas::hash_bytes(b"x"),
            text: "x".into(),
            created_at: "1970-01-01T00:00:00Z".into(),
        },
    };
    chunk.row.chunk_id = kio_index::chunking::chunk_hash(&chunk.row).unwrap();
    chunk
}

#[cfg(unix)]
#[cfg(unix)]
#[test]
fn chunk_ledger_symlink_never_reads_or_mutates_victim() {
    let temp = tempfile::tempdir().unwrap();
    let kio_dir = temp.path().join(".kio");
    fs::create_dir(&kio_dir).unwrap();
    let index = kio_dir.join("index");
    fs::create_dir_all(&index).unwrap();
    let victim = temp.path().join("victim");
    let original = b"safe record\npartial victim bytes";
    fs::write(&victim, original).unwrap();
    symlink(&victim, index.join("chunks.jsonl")).unwrap();

    assert!(parse_chunk_ledger(&kio_dir).is_err());
    assert!(truncate_torn_chunk_tail(&kio_dir).is_err());
    assert!(append_stored_chunks(&kio_dir, &[ledger_test_chunk()]).is_err());
    assert_eq!(fs::read(victim).unwrap(), original);
}

#[cfg(unix)]
#[cfg(unix)]
#[test]
fn chunk_ledger_hardlink_never_reads_or_mutates_victim() {
    let temp = tempfile::tempdir().unwrap();
    let kio_dir = temp.path().join(".kio");
    let index = kio_dir.join("index");
    fs::create_dir_all(&index).unwrap();
    let victim = temp.path().join("victim");
    let original = b"safe record\npartial victim bytes";
    fs::write(&victim, original).unwrap();
    fs::hard_link(&victim, index.join("chunks.jsonl")).unwrap();

    assert!(parse_chunk_ledger(&kio_dir).is_err());
    assert!(truncate_torn_chunk_tail(&kio_dir).is_err());
    assert!(append_stored_chunks(&kio_dir, &[ledger_test_chunk()]).is_err());
    assert_eq!(fs::read(victim).unwrap(), original);
}

#[cfg(unix)]
#[cfg(unix)]
#[test]
fn chunk_ledger_parent_symlink_never_reaches_victim_directory() {
    let temp = tempfile::tempdir().unwrap();
    let kio_dir = temp.path().join(".kio");
    fs::create_dir_all(&kio_dir).unwrap();
    let victim = temp.path().join("victim-index");
    fs::create_dir(&victim).unwrap();
    let victim_ledger = victim.join("chunks.jsonl");
    fs::write(&victim_ledger, b"victim record\n").unwrap();
    symlink(&victim, kio_dir.join("index")).unwrap();

    assert!(parse_chunk_ledger(&kio_dir).is_err());
    assert!(truncate_torn_chunk_tail(&kio_dir).is_err());
    assert!(append_stored_chunks(&kio_dir, &[ledger_test_chunk()]).is_err());
    assert_eq!(fs::read(victim_ledger).unwrap(), b"victim record\n");
}

#[cfg(unix)]
#[cfg(unix)]
#[test]
fn replacing_chunk_ledger_never_follows_index_or_ledger_symlink() {
    let temp = tempfile::tempdir().unwrap();
    let kio_dir = temp.path().join(".kio");
    fs::create_dir_all(&kio_dir).unwrap();
    let index_victim = tempfile::tempdir().unwrap();
    let index_ledger = index_victim.path().join("chunks.jsonl");
    fs::write(&index_ledger, b"index victim\n").unwrap();
    symlink(index_victim.path(), kio_dir.join("index")).unwrap();
    assert!(replace_chunk_ledger_contents(&kio_dir, b"replacement\n").is_err());
    assert_eq!(fs::read(&index_ledger).unwrap(), b"index victim\n");

    fs::remove_file(kio_dir.join("index")).unwrap();
    fs::create_dir(kio_dir.join("index")).unwrap();
    let ledger_victim = temp.path().join("ledger-victim");
    fs::write(&ledger_victim, b"ledger victim\n").unwrap();
    symlink(&ledger_victim, kio_dir.join("index/chunks.jsonl")).unwrap();
    assert!(replace_chunk_ledger_contents(&kio_dir, b"replacement\n").is_err());
    assert_eq!(fs::read(&ledger_victim).unwrap(), b"ledger victim\n");
}

#[cfg(unix)]
#[cfg(unix)]
#[test]
fn held_chunk_ledger_root_survives_kio_replacement_without_touching_victim() {
    let temp = tempfile::tempdir().unwrap();
    let kio_dir = temp.path().join(".kio");
    fs::create_dir_all(kio_dir.join("index")).unwrap();
    let directory = crate::index::chunk_ledger::open_chunk_ledger_dir(&kio_dir, false).unwrap();
    let victim = tempfile::tempdir().unwrap();
    let original = temp.path().join(".kio-original");
    fs::rename(&kio_dir, &original).unwrap();
    symlink(victim.path(), &kio_dir).unwrap();

    let mut file =
        crate::index::chunk_ledger::open_chunk_ledger_file(&directory, false, true, true, true)
            .expect("held index handle must remain usable");
    use std::io::Write;
    file.write_all(b"held-root-record\n").unwrap();
    file.sync_all().unwrap();
    drop(file);
    crate::index::chunk_ledger::sync_chunk_ledger_dir(&directory).unwrap();

    assert!(!victim.path().join("index/chunks.jsonl").exists());
    assert_eq!(
        fs::read(original.join("index/chunks.jsonl")).unwrap(),
        b"held-root-record\n"
    );
}

#[test]
fn publication_event_can_follow_its_newly_durable_creation_and_is_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    let kio_dir = temp.path().join(".kio");
    fs::create_dir(&kio_dir).unwrap();
    let chunk = ledger_test_chunk();
    let event = ChunkPublicationEvent {
        event: "publication".to_owned(),
        chunk_id: chunk.row.chunk_id.clone(),
        chunking_config_hash: chunk.row.chunking_config_hash.clone(),
        introduction_commit:
            "sha256:1111111111111111111111111111111111111111111111111111111111111111".to_owned(),
    };

    // This is the ordering used when two attested manifests introduce the
    // same semantic chunk during one rebuild: fsync creation first, then
    // append the tagged introduction.
    append_stored_chunks(&kio_dir, &[chunk]).unwrap();
    append_chunk_publication_events(&kio_dir, std::slice::from_ref(&event)).unwrap();
    append_chunk_publication_events(&kio_dir, &[event]).unwrap();

    assert_eq!(read_chunk_publication_events(&kio_dir).unwrap().len(), 1);
}

#[test]
fn publication_event_before_its_creation_is_corrupt_even_if_creation_appears_later() {
    let temp = tempfile::tempdir().unwrap();
    let kio_dir = temp.path().join(".kio");
    fs::create_dir_all(kio_dir.join("index")).unwrap();
    let chunk = ledger_test_chunk();
    let event = ChunkPublicationEvent {
        event: "publication".to_owned(),
        chunk_id: chunk.row.chunk_id.clone(),
        chunking_config_hash: chunk.row.chunking_config_hash.clone(),
        introduction_commit:
            "sha256:1111111111111111111111111111111111111111111111111111111111111111".to_owned(),
    };
    fs::write(
        kio_dir.join("index/chunks.jsonl"),
        format!(
            "{}\n{}\n",
            serde_json::to_string(&event).unwrap(),
            serde_json::to_string(&chunk).unwrap(),
        ),
    )
    .unwrap();

    let error = match parse_chunk_ledger(&kio_dir) {
        Err(error) => error,
        Ok(_) => panic!("an event before its creation must be rejected"),
    };
    assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
}

#[test]
fn duplicate_association_without_publication_does_not_read_the_chunk_ledger() {
    let temp = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp.path()).unwrap();
    let kio_dir = repo.kio_dir().to_path_buf();
    fs::create_dir_all(kio_dir.join("index")).unwrap();
    // If the duplicate branch consults the ledger, this malformed row fails
    // parsing. The normal rebuild path deliberately skips that read when it
    // is not staging a targeted duplicate publication.
    fs::write(kio_dir.join("index/chunks.jsonl"), b"not json\n").unwrap();

    let mut row = ledger_test_chunk().row;
    row.unit_content_hash = kio_core::cas::hash_bytes(b"x");
    row.chunk_id = kio_index::chunking::chunk_hash(&row).unwrap();
    let units = unit_authorities_from_inputs(&[NormalizedUnitInput {
        raw_hash: row.raw_hash.clone(),
        tool_profile_hash: row.tool_profile_hash.clone(),
        r#gen: row.r#gen,
        unit_key: row.unit_key.clone(),
        unit_content_hash: row.unit_content_hash.clone(),
        markdown: "x".to_owned(),
    }]);
    let mut known_associations =
        BTreeSet::from([(row.chunk_id.clone(), row.chunking_config_hash.clone())]);
    let mut chunk_rowids = BTreeMap::new();
    let mut next_rowid = 1;
    let mut next_association_rowid = 1;
    let mut appended = Vec::new();
    assert_eq!(
        append_new_chunk_association(
            &repo,
            row,
            &mut known_associations,
            &mut chunk_rowids,
            &mut next_rowid,
            &mut next_association_rowid,
            &mut appended,
            &units,
        )
        .unwrap(),
        AssociationAppend::Existing,
    );

    assert!(appended.is_empty());
}

#[test]
fn complete_chunk_ledger_rows_reject_missing_fields_unknown_fields_and_bad_hashes() {
    let temp = tempfile::tempdir().unwrap();
    let kio_dir = temp.path().join(".kio");
    fs::create_dir_all(kio_dir.join("index")).unwrap();
    let path = kio_dir.join("index/chunks.jsonl");
    let valid = serde_json::to_value(ledger_test_chunk()).unwrap();

    let mut non_current_field = valid.clone();
    non_current_field.as_object_mut().unwrap().insert(
        // Exact frozen negative vector for the removed pre-cutover field.
        // It is named here only to prove that no compatibility reader
        // silently accepts the obsolete ledger shape.
        "first_seen_commit".to_owned(),
        serde_json::json!("sha256:1111111111111111111111111111111111111111111111111111111111"),
    );
    fs::write(
        &path,
        format!("{}\n", serde_json::to_string(&non_current_field).unwrap()),
    )
    .unwrap();
    let error = read_stored_chunks(&kio_dir).unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");

    let mut unknown_field = valid.clone();
    unknown_field
        .as_object_mut()
        .unwrap()
        .insert("forged".to_owned(), serde_json::json!(true));
    fs::write(
        &path,
        format!("{}\n", serde_json::to_string(&unknown_field).unwrap()),
    )
    .unwrap();
    let error = read_stored_chunks(&kio_dir).unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");

    let mut bad_hash = valid;
    bad_hash.as_object_mut().unwrap().insert(
        "raw_hash".to_owned(),
        serde_json::json!("not-a-content-hash"),
    );
    fs::write(
        &path,
        format!("{}\n", serde_json::to_string(&bad_hash).unwrap()),
    )
    .unwrap();
    let error = read_stored_chunks(&kio_dir).unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
}

#[test]
fn publication_compatibility_is_strict_and_uses_preloaded_relations() {
    let mut publications = BTreeMap::new();
    publications.insert(
        "chunk".to_owned(),
        BTreeSet::from(["c1".to_owned(), "side".to_owned()]),
    );
    let ancestors = BTreeSet::from(["c0".to_owned(), "c1".to_owned(), "c2".to_owned()]);

    assert!(publication_is_compatible(
        publications.get("chunk"),
        Some(&ancestors),
    ));
    // Include-deleted uses its final appearance's ancestry, not exact
    // equality: publication can have happened before deletion.
    assert!(publication_is_compatible(
        publications.get("chunk"),
        Some(&BTreeSet::from(["c1".to_owned(), "deleted".to_owned()])),
    ));
    assert!(!publication_is_compatible(publications.get("chunk"), None));
    assert!(!publication_is_compatible(None, Some(&ancestors)));
}

#[test]
fn include_deleted_publications_are_ranked_on_one_chain() {
    let mut parents = BTreeMap::from([
        ("head".to_owned(), Some("p2".to_owned())),
        ("p2".to_owned(), Some("p1".to_owned())),
        ("p1".to_owned(), Some("root".to_owned())),
        ("root".to_owned(), None),
    ]);
    let mut child = "root".to_owned();
    for index in 0..128 {
        let commit = format!("old-{index:03}");
        parents.insert(child.clone(), Some(commit.clone()));
        parents.insert(commit.clone(), None);
        child = commit;
    }
    let ranks = publication_ranks("head", &parents);
    assert_eq!(ranks.len(), 132);
    assert_eq!(ranks.get("head"), Some(&0));
    assert_eq!(ranks.get("p1"), Some(&2));
    assert!(publication_is_include_deleted_compatible(
        Some(&BTreeSet::from(["root".to_owned()])),
        ranks.get("p1"),
        &ranks,
    ));
    for forbidden in ["head", "unrelated"] {
        assert!(!publication_is_include_deleted_compatible(
            Some(&BTreeSet::from([forbidden.to_owned()])),
            ranks.get("p1"),
            &ranks,
        ));
    }
}

#[test]
fn replica_write_through_refuses_a_missing_source_index_stamp() {
    let root = tempfile::tempdir().unwrap();
    let kio_dir = root.path().join(".kio");
    std::fs::create_dir_all(&kio_dir).unwrap();
    std::fs::write(
        kio_dir.join("scope.json"),
        r#"{"scope_id":"scope_test_missing_stamp"}"#,
    )
    .unwrap();

    let error = write_through_projection_with_requested_at(
        &kio_dir,
        &[],
        &kio_core::control::ControlHandle::default(),
    )
    .unwrap_err();
    assert!(
        error.contains("source index stamp is unavailable"),
        "a writer must not silently retain a Ready replica when its source index is gone: {error}"
    );
    assert!(
        !kio_dir.join("index/sqlite.db").exists(),
        "replica repair must not create a missing source index"
    );
}

#[test]
fn replica_write_through_refuses_an_unresolvable_scope_identity() {
    let root = tempfile::tempdir().unwrap();
    let kio_dir = root.path().join(".kio");
    std::fs::create_dir_all(&kio_dir).unwrap();

    let error = write_through_projection_with_requested_at(
        &kio_dir,
        &[],
        &kio_core::control::ControlHandle::default(),
    )
    .unwrap_err();
    assert!(
        error.contains("source index stamp is unavailable"),
        "an unresolvable scope must never turn a failed projection into a successful no-op: {error}"
    );
}

#[test]
fn adapter_scope_id_refuses_malformed_scope_identity_instead_of_unknown() {
    let root = tempfile::tempdir().unwrap();
    let repo = kio_core::scope::Repository::init(root.path()).unwrap();
    fs::write(repo.kio_dir().join("scope.json"), "{not valid json").unwrap();

    let error = repo.scope_identity().unwrap_err();
    assert_ne!(error.to_string(), "unknown");
}

/// R23-13 (06 §7 L343-344 "KIO-E-STORE-CONSTRAINT-001 ... permanent・
/// 非再試行で command を即時中止・exit 4"): `pipeline_to_kio` maps this ONE
/// `Contract` code to `PermanentFailure` (4), while every OTHER
/// `Contract` code (scan.rs/prepare.rs/markdownize.rs's contract
/// violations) keeps the generic `Failure` (1) it had before the fix —
/// scoped, not a blanket exit-4 promotion for the whole variant.
#[test]
fn r23_13_store_constraint_contract_error_is_permanent_failure_exit_4() {
    use crate::object_persistence::pipeline_to_kio;

    let constraint = pipeline_to_kio(kio_pipeline::PipelineError::contract(
        "KIO-E-STORE-CONSTRAINT-001",
        "cost_ledger/batch_requests CHECK constraint violated",
    ));
    assert_eq!(constraint.error_code(), "KIO-E-STORE-CONSTRAINT-001");
    assert_eq!(constraint.exit_code(), kio_core::ExitCode::PermanentFailure);

    let other = pipeline_to_kio(kio_pipeline::PipelineError::contract(
        "KIO-E-ADAPTER-CONTRACT-001",
        "adapter response failed its acceptance check",
    ));
    assert_eq!(other.error_code(), "KIO-E-ADAPTER-CONTRACT-001");
    assert_eq!(
        other.exit_code(),
        kio_core::ExitCode::Failure,
        "only KIO-E-STORE-CONSTRAINT-001 is promoted to exit 4, not every Contract code"
    );
}

#[test]
fn ct4_rebuild_refuses_visible_purge_and_filters_its_raw() {
    use crate::scope_access::ensure_no_visible_purge_journal;
    use crate::scope_access::purge_blocks_rebuild_raw;
    use kio_core::purge::{BeginOutcome, PurgePhase, PurgeReason, PurgeState, TombstoneMode};

    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let kio_dir = repo.kio_dir().to_path_buf();
    let raw_hash = kio_pipeline::prepare::hash_bytes(b"purge barrier rebuild target");
    let state = PurgeState::open(&kio_dir).unwrap();
    let journal = match state
        .begin(
            vec![raw_hash.clone()],
            PurgeReason::Privacy,
            TombstoneMode::Default,
            "user",
            "2026-07-13T00:00:00Z",
            1,
            kio_pipeline::prepare::hash_bytes(b"planned purge commit"),
            kio_pipeline::prepare::hash_bytes(b"planned purge closure"),
            kio_core::scope::new_ulid(dir.path()),
        )
        .unwrap()
    {
        BeginOutcome::Started(journal) => journal,
        other => panic!("unexpected begin outcome: {other:?}"),
    };

    // Prepared is not yet public. Once the barrier is durable, every rebuild
    // entry point fails before writing and its target is filtered per raw.
    ensure_no_visible_purge_journal(&kio_dir, &ControlHandle::default()).unwrap();
    assert!(!purge_blocks_rebuild_raw(&kio_dir, &raw_hash, &ControlHandle::default()).unwrap());
    state
        .advance_phase(&journal, PurgePhase::Tombstoned)
        .unwrap();
    let error = ensure_no_visible_purge_journal(&kio_dir, &ControlHandle::default()).unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-PURGE-INCOMPLETE-001");
    assert!(purge_blocks_rebuild_raw(&kio_dir, &raw_hash, &ControlHandle::default()).unwrap());
}

#[test]
fn r23_10_purge_blocks_historical_reindex_raw_uses_canonical_not_existence() {
    use crate::scope_access::purge_blocks_historical_reindex_raw;
    use kio_core::purge::{LifecycleEvent, PurgeReason, PurgeState};

    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let kio_dir = repo.kio_dir().to_path_buf();
    let state = PurgeState::open(&kio_dir).unwrap();

    // A canonical `purged` tombstone blocks.
    let purged_raw = kio_pipeline::prepare::hash_bytes(b"purged only");
    state
        .append_tombstone_event(
            &purged_raw,
            LifecycleEvent::purged(
                "2026-07-13T00:00:00Z",
                kio_pipeline::prepare::hash_bytes(b"purge commit"),
                PurgeReason::Legal,
                "user",
                1,
            ),
        )
        .unwrap();
    assert!(
        purge_blocks_historical_reindex_raw(&kio_dir, &purged_raw, &ControlHandle::default())
            .unwrap()
    );

    // AUD-08 scenario 1: the SAME tombstone later retired (resurrection)
    // -- canonical is now `retired` (alive), so it must stop blocking,
    // unlike the pre-fix `read_tombstone(...).is_some()` (existence
    // only) check that excluded it forever after its first purge.
    state
        .retire_tombstone(
            &purged_raw,
            &kio_pipeline::prepare::hash_bytes(b"resurrection commit"),
            "2026-07-14T00:00:00Z",
            "user",
        )
        .unwrap();
    assert!(
        !purge_blocks_historical_reindex_raw(&kio_dir, &purged_raw, &ControlHandle::default())
            .unwrap()
    );

    // AUD-08 scenario 2: a tombstone's tail is `purged`, but a
    // CO-EXISTING erase receipt's tail is `retired` at a HIGHER
    // lifecycle_epoch (the receipt was independently resurrected later)
    // -- canonical (max-epoch across both markers) is the receipt's
    // `retired`, so this must NOT block, even though the tombstone
    // alone still looks active in isolation.
    let coexist_raw = kio_pipeline::prepare::hash_bytes(b"tombstone and receipt coexist");
    state
        .append_tombstone_event(
            &coexist_raw,
            LifecycleEvent::purged(
                "2026-07-13T00:00:00Z",
                kio_pipeline::prepare::hash_bytes(b"purge commit 2"),
                PurgeReason::Legal,
                "user",
                1,
            ),
        )
        .unwrap();
    state
        .append_erase_receipt_event(
            &coexist_raw,
            LifecycleEvent::erased(
                "2026-07-13T00:00:00Z",
                kio_pipeline::prepare::hash_bytes(b"erase commit"),
                PurgeReason::Legal,
                "user",
                1,
            ),
        )
        .unwrap();
    state
        .retire_erase_receipt(
            &coexist_raw,
            &kio_pipeline::prepare::hash_bytes(b"resurrection via receipt"),
            "2026-07-15T00:00:00Z",
            "user",
        )
        .unwrap();
    // The tombstone alone (in isolation) still looks active...
    assert!(
        state
            .read_tombstone(&coexist_raw)
            .unwrap()
            .unwrap()
            .is_active()
    );
    // ...but canonical across both markers is the receipt's
    // higher-epoch `retired`, so this must not block.
    assert!(
        !purge_blocks_historical_reindex_raw(&kio_dir, &coexist_raw, &ControlHandle::default())
            .unwrap()
    );

    // An erase receipt with no tombstone at all -- canonical `erased`,
    // not `purged` -- must NOT block (erase receipts are not a
    // re-ingest/visibility barrier outside their closed use-list,
    // 08-evidence-pointer-spec.md §4.2).
    let erased_only_raw = kio_pipeline::prepare::hash_bytes(b"erased only");
    state
        .append_erase_receipt_event(
            &erased_only_raw,
            LifecycleEvent::erased(
                "2026-07-13T00:00:00Z",
                kio_pipeline::prepare::hash_bytes(b"erase commit 2"),
                PurgeReason::Privacy,
                "user",
                1,
            ),
        )
        .unwrap();
    assert!(
        !purge_blocks_historical_reindex_raw(&kio_dir, &erased_only_raw, &ControlHandle::default())
            .unwrap()
    );
}

#[test]
fn r23_10_erase_receipt_blocks_retained_embedding_liveness() {
    // R23-10 (05-runtime.md §3.5 L934/L813, AUD-08's shrunk finding):
    // this test used to be named `..._is_not_retained_embedding_liveness`
    // and asserted the OPPOSITE (`chunks.len() == 1`) — it pinned the
    // pre-fix bug where `retained_history_chunks` only checked tombstone
    // EXISTENCE (`purge.read_tombstone(...).is_some()`), which is `None`
    // for a raw whose only marker is an erase receipt, so its text kept
    // flowing into embedding generation. That is a privacy-relevant gap:
    // content erased for legal/privacy reasons must not be sent to an
    // external embedding provider. `purge_blocks_rebuild_raw`'s
    // canonical-final-event check (`kio_core::purge::canonical_final_event`,
    // combining both markers) is the same predicate every other
    // rebuild/embedding call site on this path already uses
    // (`historical_reindex::retained_history_instances`,
    // `historical_reindex::project_selected_snapshot`).
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let kio_dir = repo.kio_dir().to_path_buf();
    let raw_hash = kio_pipeline::prepare::hash_bytes(b"explicitly reintroduced raw");
    let profile_hash = kio_pipeline::prepare::hash_bytes(b"profile");
    let commit_hash = kio_pipeline::prepare::hash_bytes(b"commit");
    let purge = kio_core::purge::PurgeState::open(&kio_dir).unwrap();
    purge
        .append_erase_receipt_event(
            &raw_hash,
            kio_core::purge::LifecycleEvent::erased(
                "2026-07-13T00:00:00Z",
                commit_hash.clone(),
                kio_core::purge::PurgeReason::Legal,
                "user",
                1,
            ),
        )
        .unwrap();

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE chunks (
                 rowid INTEGER PRIMARY KEY,
                 chunk_id TEXT NOT NULL,
                 text TEXT NOT NULL,
                 text_hash TEXT NOT NULL,
                 raw_hash TEXT NOT NULL,
                 tool_profile_hash TEXT NOT NULL,
                 gen INTEGER NOT NULL
             );
             CREATE TABLE chunk_config_generations (
                 chunk_id TEXT NOT NULL,
                 chunking_config_hash TEXT NOT NULL
             );
             CREATE TABLE chunk_publications (
                 chunk_id TEXT NOT NULL,
                 chunking_config_hash TEXT NOT NULL,
                 introduction_commit TEXT NOT NULL
             );",
    )
    .unwrap();
    let chunk_id = kio_pipeline::prepare::hash_bytes(b"chunk");
    let text_hash = kio_pipeline::prepare::hash_bytes(b"searchable text");
    conn.execute(
        "INSERT INTO chunks
             (chunk_id,text,text_hash,raw_hash,tool_profile_hash,gen)
             VALUES (?1,'searchable text',?2,?3,?4,0)",
        rusqlite::params![chunk_id, text_hash, raw_hash, profile_hash],
    )
    .unwrap();
    conn.execute(
            "INSERT INTO chunk_publications(chunk_id,chunking_config_hash,introduction_commit) VALUES (?1,'config',?2)",
            rusqlite::params![chunk_id, commit_hash],
        )
        .unwrap();
    conn.execute(
        "INSERT INTO chunk_config_generations(chunk_id,chunking_config_hash)
             VALUES (?1,'config')",
        rusqlite::params![chunk_id],
    )
    .unwrap();
    let retained = vec![crate::historical_reindex::RetainedNormalizedInstance {
        raw_hash,
        normalize: kio_core::dag::NormalizeRef {
            tool_profile_hash: profile_hash,
            r#gen: 0,
            manifest_hash: kio_core::cas::hash_bytes(b"manifest"),
        },
        raw_path: "reintroduced.md".to_owned(),
        policy_paths: BTreeSet::from(["reintroduced.md".to_owned()]),
        introductions: vec![commit_hash],
    }];
    let chunks = crate::embedding::enrichment::retained_history_chunks(
        &conn, &repo, &retained, "config", None,
    )
    .unwrap();
    assert_eq!(chunks.len(), 0);
}

#[cfg(debug_assertions)]
#[test]
fn search_index_status_uses_preflight_month() {
    use crate::index::command::compute_index_status;
    use crate::search::scope::SearchedScopeInfo;
    use crate::settings::user_config_toml_path;
    use kio_core::scope::Repository;
    use kio_core::test_control::DebugTestControl;
    use kio_pipeline::budget::utc_month;
    use kio_pipeline::ledger::LedgerDb;
    use kio_pipeline::ledger::LedgerReadSnapshot;
    use std::path::PathBuf;

    // Re-exec only this unit test with private HOME/XDG/TMP roots so its
    // budget-policy reads cannot depend on the user's config. This is
    // test-harness code, not a production clock or authorization seam.
    if std::env::var_os("KIO_TEST_BUDGET_MONTH_CHILD").is_none() {
        let dir = crate::test_support::PrivateTempDir::new();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::search_index_status_uses_preflight_month"])
            .env_clear()
            .env("KIO_TEST_BUDGET_MONTH_CHILD", dir.path())
            .env("HOME", dir.path())
            .env("XDG_CONFIG_HOME", dir.path().join("config"))
            .env("XDG_DATA_HOME", dir.path().join("data"))
            .env("XDG_CACHE_HOME", dir.path().join("cache"))
            .env("TMPDIR", dir.path())
            .env("TMP", dir.path())
            .env("TEMP", dir.path())
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        return;
    }

    let private_root = PathBuf::from(std::env::var_os("KIO_TEST_BUDGET_MONTH_CHILD").unwrap());
    assert_eq!(
        std::env::current_dir().unwrap().canonicalize().unwrap(),
        private_root.canonicalize().unwrap()
    );
    for (name, expected) in [
        ("HOME", private_root.clone()),
        ("XDG_CONFIG_HOME", private_root.join("config")),
        ("XDG_DATA_HOME", private_root.join("data")),
        ("XDG_CACHE_HOME", private_root.join("cache")),
    ] {
        assert_eq!(std::env::var_os(name), Some(expected.into_os_string()));
    }
    let dir = crate::test_support::PrivateTempDir::new();
    let repo = Repository::init(dir.path()).unwrap();
    let scope_id = repo.scope_identity().unwrap().scope_id;
    let device_config = user_config_toml_path(&kio_core::control::ControlHandle::default());
    fs::create_dir_all(device_config.parent().unwrap()).unwrap();
    fs::write(&device_config, "[budget]\nmonthly_usd_cap = 100.0\n").unwrap();
    let folder_config = repo.kio_dir().join("config.toml");
    let mut config: toml_edit::DocumentMut =
        fs::read_to_string(&folder_config).unwrap().parse().unwrap();
    config["budget"]["monthly_usd_cap"] = toml_edit::value(1.0);
    fs::write(&folder_config, config.to_string()).unwrap();

    let ledger_path = dir.path().join("device/cost-ledger.sqlite");
    drop(LedgerDb::initialize(&ledger_path).unwrap());
    rusqlite::Connection::open(&ledger_path)
        .unwrap()
        .execute(
            "INSERT INTO cost_ledger (
                scope_id, adapter_kind, input_hash, tool_profile_hash, submission_seq,
                batch_job_id, usd, estimated, outcome, month, recorded_at
             ) VALUES (?1, 'embedding', 'month-input', 'month-profile', 1,
                'month-job', 1.0, 0, 'succeeded', '2026-09', ?2)",
            rusqlite::params![
                scope_id,
                kio_pipeline::ledger::time::month_start_millis(2026, 9)
            ],
        )
        .unwrap();
    let snapshot = LedgerReadSnapshot::open(&ledger_path).unwrap();
    let searched = [SearchedScopeInfo {
        scope_id,
        scope_path: repo.kio_dir().to_path_buf(),
        repo_root: dir.path().to_path_buf(),
        snapshot_at: "sha256:month".to_owned(),
        max_rowid: 0,
        max_association_rowid: 0,
        chunking_config_hash: "sha256:config".to_owned(),
        index_generation: "01TEST0000000000000000000".to_owned(),
        journal_active_at_prepare: false,
        shallow_skipped: 0,
        runtime_binding_filter: None,
    }];
    let clock = kio_core::control::ControlHandle::new(DebugTestControl {
        fixed_now: kio_core::test_control::Selector::Known("2026-10-01T00:00:00Z".to_owned()),
        ..Default::default()
    });
    // The response clock is October, but September's preflight month
    // must still govern both folder and device exhaustion observations.
    assert_eq!(utc_month(&clock.now_utc_seconds()), "2026-10");
    for device_only in [false, true] {
        if device_only {
            fs::write(&device_config, "[budget]\nmonthly_usd_cap = 1.0\n").unwrap();
            config["budget"]["monthly_usd_cap"] = toml_edit::value(100.0);
            fs::write(&folder_config, config.to_string()).unwrap();
        }
        for (month, paused) in [("2026-09", true), ("2026-10", false)] {
            let status = compute_index_status(
                &searched,
                Some(&snapshot),
                &ledger_path,
                month,
                &kio_core::control::ControlHandle::default(),
                &kio_core::scope::LockSession::default(),
            )
            .unwrap();
            assert_eq!(
                status.budget_paused, paused,
                "device_only={device_only} month={month}"
            );
        }
    }
}

#[test]
fn search_budget_observation_retries_a_month_rollover_once() {
    use crate::ledger_access::capture_search_budget_observation_with;
    use kio_pipeline::ledger::LedgerDb;

    let root = crate::test_support::PrivateTempDir::new();
    let ledger_path = root.path().join("device/cost-ledger.sqlite");
    drop(LedgerDb::initialize(&ledger_path).unwrap());
    let ticks = std::cell::RefCell::new(
        [
            "2026-09-30T23:59:59Z",
            "2026-10-01T00:00:00Z",
            "2026-10-01T00:00:00Z",
            "2026-10-01T00:00:01Z",
        ]
        .into_iter(),
    );
    let (month, snapshot) = capture_search_budget_observation_with(
        &ledger_path,
        || ticks.borrow_mut().next().unwrap().to_owned(),
        &ControlHandle::default(),
    )
    .unwrap();

    assert_eq!(month, "2026-10");
    assert!(snapshot.is_some());

    let exhausted_ticks = std::cell::RefCell::new(
        [
            "2026-10-31T23:59:59Z",
            "2026-11-01T00:00:00Z",
            "2026-11-30T23:59:59Z",
            "2026-12-01T00:00:00Z",
        ]
        .into_iter(),
    );
    let error = capture_search_budget_observation_with(
        &ledger_path,
        || exhausted_ticks.borrow_mut().next().unwrap().to_owned(),
        &ControlHandle::default(),
    )
    .err()
    .expect("rollover retry exhaustion must fail");
    assert_eq!(error.error_code(), "KIO-E-LEDGER-SNAPSHOT-ROLLOVER-001");
    assert_eq!(error.exit_code(), kio_core::ExitCode::PartialFailure);
}

#[test]
fn r23_aggregate_index_status_fails_closed_for_corrupt_scope_state() {
    use crate::index::command::compute_index_status;
    use crate::search::scope::SearchedScopeInfo;
    use kio_core::scope::Repository;

    let healthy_root = tempfile::tempdir().unwrap();
    let corrupt_root = tempfile::tempdir().unwrap();
    let vanished_root = tempfile::tempdir().unwrap();
    Repository::init(healthy_root.path()).unwrap();
    let corrupt_repo = Repository::init(corrupt_root.path()).unwrap();
    std::fs::write(corrupt_repo.kio_dir().join("tasks.jsonl"), b"not-json\n").unwrap();
    std::fs::write(
        corrupt_repo.kio_dir().join("unsupported-inputs.jsonl"),
        b"not-json\n",
    )
    .unwrap();

    // R23-20: `SearchedScopeInfo.scope_path` is the canonical `.kio`
    // directory; `repo_root` (added by the same fix) carries the parent
    // scope root `compute_index_status` needs for `Repository::open`.
    let searched = [
        SearchedScopeInfo {
            scope_id: "healthy".to_owned(),
            scope_path: healthy_root.path().join(".kio"),
            repo_root: healthy_root.path().to_path_buf(),
            snapshot_at: "sha256:healthy".to_owned(),
            max_rowid: 0,
            max_association_rowid: 0,
            chunking_config_hash: "sha256:config".to_owned(),
            index_generation: "01TEST0000000000000000000".to_owned(),
            journal_active_at_prepare: false,
            shallow_skipped: 0,
            runtime_binding_filter: None,
        },
        SearchedScopeInfo {
            scope_id: "corrupt".to_owned(),
            scope_path: corrupt_root.path().join(".kio"),
            repo_root: corrupt_root.path().to_path_buf(),
            snapshot_at: "sha256:corrupt".to_owned(),
            max_rowid: 0,
            max_association_rowid: 0,
            chunking_config_hash: "sha256:config".to_owned(),
            index_generation: "01TEST0000000000000000000".to_owned(),
            journal_active_at_prepare: false,
            shallow_skipped: 0,
            runtime_binding_filter: None,
        },
        SearchedScopeInfo {
            scope_id: "vanished".to_owned(),
            scope_path: vanished_root.path().join(".kio"),
            repo_root: vanished_root.path().to_path_buf(),
            snapshot_at: "sha256:vanished".to_owned(),
            max_rowid: 0,
            max_association_rowid: 0,
            chunking_config_hash: "sha256:config".to_owned(),
            index_generation: "01TEST0000000000000000000".to_owned(),
            journal_active_at_prepare: false,
            shallow_skipped: 0,
            runtime_binding_filter: None,
        },
    ];

    let status = compute_index_status(
        &searched,
        None,
        std::path::Path::new("/private/test/cost-ledger.sqlite"),
        "2026-09",
        &kio_core::control::ControlHandle::default(),
        &kio_core::scope::LockSession::default(),
    )
    .unwrap();
    assert!(!status.tasks_complete);
    assert!(!status.unsupported_inputs_complete);
    assert_eq!(status.task_errors.len(), 2);
    assert_eq!(status.unsupported_input_errors.len(), 2);
    for errors in [&status.task_errors, &status.unsupported_input_errors] {
        assert!(
            errors
                .iter()
                .any(|error| { error.scope_path == vanished_root.path().join(".kio") })
        );
    }
    assert!(status.task_errors[0].error_code.contains("STORE-CORRUPT"));
    assert!(
        status.unsupported_input_errors[0]
            .error_code
            .contains("STORE-CORRUPT")
    );
}

#[test]
fn r23_cand_001_terminal_failure_does_not_spawn_releasable_secret_hold() {
    use crate::egress::prune_terminal_embedding_secret_holds;
    use crate::embedding::enrichment::embedding_task_output_ref;
    use crate::embedding::enrichment::hold_secret_embedding_tasks;
    use kio_adapter::catalog::DeclaredEmbeddingProfile;
    use kio_core::scope::Repository;
    use kio_pipeline::ledger::LedgerDb;
    use kio_pipeline::task::{TaskDescriptor, TaskStatus, TaskStore, TaskType};

    let root = crate::test_support::PrivateTempDir::new();
    let repo = Repository::init(root.path()).unwrap();
    let store = TaskStore::new(repo.kio_dir());
    let ledger = LedgerDb::initialize(root.path().join("device/cost-ledger.sqlite")).unwrap();
    let profile = DeclaredEmbeddingProfile {
        tool_id: "test_embedding".to_owned(),
        dimensions: 768,
        distance: "cosine".to_owned(),
        modality: "multimodal".to_owned(),
        profile_hash: format!("sha256:{}", "e".repeat(64)),
    };
    let chunk_id = format!("sha256:{}", "a".repeat(64));
    let chunk = crate::embedding::enrichment::bind_embedding_work(
        vec![crate::embedding_owners::RetainedEmbeddingChunk {
            chunk_id: chunk_id.clone(),
            text: "secret".into(),
            text_hash: format!("sha256:{}", "b".repeat(64)),
            raw_path: "credentials_backup.md".to_owned(),
            requires_secret_approval: true,
            is_head_owner: false,
        }],
        &profile,
    )
    .unwrap()
    .remove(0);
    let output_ref = embedding_task_output_ref(&chunk);
    store
        .append(&TaskDescriptor {
            task_id: "task_terminal".to_owned(),
            task_type: TaskType::Embedding,
            mode: None,
            input_path: "credentials_backup.md".to_owned(),
            input_hash: format!("sha256:{}", "b".repeat(64)),
            previous_raw_hash: None,
            parent_run_id: None,
            changed_unit_keys: vec![chunk_id.clone()],
            output_ref: output_ref.clone(),
            unit_keys: None,
            status: TaskStatus::Failed,
            attempts: 1,
            next_retry_at: None,
            deadline: None,
            heartbeat_at: None,
            fallback_reason: Some("contract_violation".to_owned()),
            created_at: "2026-07-12T00:00:00Z".to_owned(),
            bbox_annotation_enabled: None,
            hold_reason: None,
            reserved_usd: None,
            reserved_month: None,
            reservation_id: None,
        })
        .unwrap();

    hold_secret_embedding_tasks(
        &store,
        &repo,
        Some(&ledger),
        &profile,
        std::slice::from_ref(&chunk),
        "2026-07-12T00:00:01Z",
    )
    .unwrap();

    let tasks = store.all().unwrap();
    assert_eq!(
        tasks.len(),
        1,
        "terminal state must not gain a duplicate hold"
    );
    assert_eq!(tasks[0].output_ref, output_ref);
    assert_eq!(tasks[0].status, TaskStatus::Failed);
    assert_eq!(
        tasks[0].fallback_reason.as_deref(),
        Some("contract_violation")
    );
    assert_eq!(prune_terminal_embedding_secret_holds(&repo).unwrap(), 0);
    assert_eq!(store.all().unwrap()[0].status, TaskStatus::Failed);
}

#[test]
fn r23_cand_001_duplicate_hold_cannot_override_terminal_failure() {
    use crate::egress::SECRETS_TIER_B_HOLD;
    use crate::egress::prune_terminal_embedding_secret_holds;
    use crate::embedding::enrichment::embedding_task_output_ref;
    use crate::embedding::transport::filter_embeddable_by_task_state;
    use kio_core::scope::Repository;
    use kio_pipeline::task::{HoldReason, TaskDescriptor, TaskStatus, TaskStore, TaskType};

    let root = tempfile::tempdir().unwrap();
    let repo = Repository::init(root.path()).unwrap();
    let store = TaskStore::new(repo.kio_dir());
    let chunk_id = format!("sha256:{}", "c".repeat(64));
    let profile = kio_adapter::catalog::DeclaredEmbeddingProfile {
        tool_id: "test_embedding".into(),
        dimensions: 768,
        distance: "cosine".into(),
        modality: "multimodal".into(),
        profile_hash: format!("sha256:{}", "e".repeat(64)),
    };
    let chunk = crate::embedding::enrichment::bind_embedding_work(
        vec![crate::embedding_owners::RetainedEmbeddingChunk {
            chunk_id: chunk_id.clone(),
            text: "must not send".into(),
            text_hash: format!("sha256:{}", "d".repeat(64)),
            raw_path: "credentials_backup.md".to_owned(),
            requires_secret_approval: true,
            is_head_owner: false,
        }],
        &profile,
    )
    .unwrap()
    .remove(0);
    let output_ref = embedding_task_output_ref(&chunk);
    let terminal = TaskDescriptor {
        task_id: "task_terminal".to_owned(),
        task_type: TaskType::Embedding,
        mode: None,
        input_path: "credentials_backup.md".to_owned(),
        input_hash: format!("sha256:{}", "d".repeat(64)),
        previous_raw_hash: None,
        parent_run_id: None,
        changed_unit_keys: vec![chunk_id.clone()],
        output_ref: output_ref.clone(),
        unit_keys: None,
        status: TaskStatus::Failed,
        attempts: 1,
        next_retry_at: None,
        deadline: None,
        heartbeat_at: None,
        fallback_reason: Some("contract_violation".to_owned()),
        created_at: "2026-07-12T00:00:00Z".to_owned(),
        bbox_annotation_enabled: None,
        hold_reason: None,
        reserved_usd: None,
        reserved_month: None,
        reservation_id: None,
    };
    let mut later_hold = terminal.clone();
    later_hold.task_id = "task_later_hold".to_owned();
    later_hold.status = TaskStatus::Paused;
    later_hold.attempts = 0;
    later_hold.fallback_reason = Some(SECRETS_TIER_B_HOLD.to_owned());
    later_hold.hold_reason = Some(HoldReason::TierBApproval);
    later_hold.created_at = "2026-07-12T00:00:01Z".to_owned();
    store.append(&terminal).unwrap();
    store.append(&later_hold).unwrap();

    assert_eq!(prune_terminal_embedding_secret_holds(&repo).unwrap(), 1);
    let tasks = store.all().unwrap();
    assert_eq!(tasks.len(), 1, "duplicate hold should be removed");
    assert!(tasks.iter().any(|task| {
        task.task_id == "task_terminal"
            && task.status == TaskStatus::Failed
            && task.fallback_reason.as_deref() == Some("contract_violation")
    }));

    let mut poisoned_pending = terminal.clone();
    poisoned_pending.task_id = "zzzz_later_pending".to_owned();
    poisoned_pending.status = TaskStatus::Pending;
    poisoned_pending.fallback_reason = None;
    poisoned_pending.created_at = "2026-07-12T00:00:02Z".to_owned();
    store.append(&poisoned_pending).unwrap();

    let sendable = filter_embeddable_by_task_state(
        &store,
        vec![chunk.clone()],
        false,
        &kio_core::control::ControlHandle::default(),
    )
    .unwrap();
    assert!(sendable.is_empty());
    assert_eq!(tasks[0].output_ref, output_ref);
}

#[test]
fn r23_secret_classification_preserves_retry_backoff_state() {
    use crate::embedding::enrichment::embedding_task_output_ref;
    use crate::embedding::enrichment::hold_secret_embedding_tasks;
    use kio_adapter::catalog::DeclaredEmbeddingProfile;
    use kio_core::scope::Repository;
    use kio_pipeline::ledger::LedgerDb;
    use kio_pipeline::task::{TaskDescriptor, TaskStatus, TaskStore, TaskType};

    let root = crate::test_support::PrivateTempDir::new();
    let repo = Repository::init(root.path()).unwrap();
    let store = TaskStore::new(repo.kio_dir());
    let ledger = LedgerDb::initialize(root.path().join("device/cost-ledger.sqlite")).unwrap();
    let profile = DeclaredEmbeddingProfile {
        tool_id: "test_embedding".to_owned(),
        dimensions: 768,
        distance: "cosine".to_owned(),
        modality: "multimodal".to_owned(),
        profile_hash: format!("sha256:{}", "e".repeat(64)),
    };
    let chunk_id = format!("sha256:{}", "e".repeat(64));
    let chunk = crate::embedding::enrichment::bind_embedding_work(
        vec![crate::embedding_owners::RetainedEmbeddingChunk {
            chunk_id: chunk_id.clone(),
            text: "retryable".into(),
            text_hash: format!("sha256:{}", "f".repeat(64)),
            raw_path: "credentials_backup.md".to_owned(),
            requires_secret_approval: true,
            is_head_owner: false,
        }],
        &profile,
    )
    .unwrap()
    .remove(0);
    let output_ref = embedding_task_output_ref(&chunk);
    store
        .append(&TaskDescriptor {
            task_id: "task_retry_backoff".to_owned(),
            task_type: TaskType::Embedding,
            mode: None,
            input_path: "credentials_backup.md".to_owned(),
            input_hash: format!("sha256:{}", "f".repeat(64)),
            previous_raw_hash: None,
            parent_run_id: None,
            changed_unit_keys: vec![chunk_id.clone()],
            output_ref,
            unit_keys: None,
            status: TaskStatus::Failed,
            attempts: 3,
            next_retry_at: Some("2099-01-01T00:00:00Z".to_owned()),
            deadline: None,
            heartbeat_at: None,
            fallback_reason: Some("network_error".to_owned()),
            created_at: "2026-07-12T00:00:00Z".to_owned(),
            bbox_annotation_enabled: None,
            hold_reason: None,
            reserved_usd: None,
            reserved_month: None,
            reservation_id: None,
        })
        .unwrap();

    hold_secret_embedding_tasks(
        &store,
        &repo,
        Some(&ledger),
        &profile,
        std::slice::from_ref(&chunk),
        "2026-07-12T00:00:01Z",
    )
    .unwrap();

    let tasks = store.all().unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].status, TaskStatus::Failed);
    assert_eq!(tasks[0].attempts, 3);
    assert_eq!(
        tasks[0].next_retry_at.as_deref(),
        Some("2099-01-01T00:00:00Z")
    );
    assert_eq!(tasks[0].fallback_reason.as_deref(), Some("network_error"));
}

#[test]
fn r23_cand_057_scan_budget_charges_failures_and_caps_empty_entries() {
    use crate::open_view::WorkingTreeScanBudget;

    let mut failed = WorkingTreeScanBudget {
        remaining_bytes: 10,
        remaining_entries: 2,
    };
    assert!(failed.consume_entry());
    let attempt = failed.reserve_file(u64::MAX).unwrap();
    assert_eq!(attempt.max_bytes, 9);
    assert_eq!(attempt.reserved_bytes, 10);
    assert_eq!(failed.remaining_bytes, 0);
    assert!(failed.reserve_file(0).is_none());

    let mut empty = WorkingTreeScanBudget {
        remaining_bytes: 10,
        remaining_entries: 2,
    };
    for _ in 0..2 {
        assert!(empty.consume_entry());
        let attempt = empty.reserve_file(0).unwrap();
        assert_eq!(attempt.max_bytes, 0);
        empty.finish_success(&attempt, 0);
        assert_eq!(empty.remaining_bytes, 10);
    }
    assert!(!empty.consume_entry());

    let mut valid = WorkingTreeScanBudget {
        remaining_bytes: 10,
        remaining_entries: 1,
    };
    assert!(valid.consume_entry());
    let attempt = valid.reserve_file(4).unwrap();
    valid.finish_success(&attempt, 4);
    assert_eq!(valid.remaining_bytes, 6);
}

/// 04 §4.3: `chunk_vec` / `image_vec` are one vector space split by a
/// sqlite-vec constraint. Ranking each table on its own would hand the
/// nearest image rank 1 next to the nearest chunk, whatever its distance;
/// merging on distance first is what makes the lane mean one thing.
#[test]
fn merge_vector_lane_ranks_both_tables_on_one_distance_order() {
    use crate::search::query::merge_vector_lane;

    let chunks = vec![
        ("sha256:chunk-near".to_owned(), 0.10),
        ("sha256:chunk-far".to_owned(), 0.90),
    ];
    let images = vec![("sha256:image-mid".to_owned(), 0.50)];
    let ranks = merge_vector_lane(chunks.clone(), images, 200);
    assert_eq!(
        ranks
            .iter()
            .map(|rank| (rank.chunk_hash.as_str(), rank.rank))
            .collect::<Vec<_>>(),
        vec![
            ("sha256:chunk-near", 1),
            ("sha256:image-mid", 2),
            ("sha256:chunk-far", 3),
        ]
    );

    // A corpus with no image vectors must produce exactly what it produced
    // before images existed — same ids, same dense ranks, no re-sort.
    let unchanged = merge_vector_lane(chunks, Vec::new(), 200);
    assert_eq!(
        unchanged
            .iter()
            .map(|rank| (rank.chunk_hash.as_str(), rank.rank))
            .collect::<Vec<_>>(),
        vec![("sha256:chunk-near", 1), ("sha256:chunk-far", 2)]
    );
}

/// U5 / §4.2 問題 A: without a text term an image can never reach the score
/// of a chunk that appears on both lanes, however close its vector is. It
/// inherits the rank of the chunk that cites it — inserted next to that
/// chunk, so `fuse_rrf`'s `take(candidate_depth)` window cannot drop it.
#[test]
fn images_take_the_text_rank_of_their_citing_chunk() {
    use crate::search::query::ImageCandidate;
    use crate::search::query::give_images_their_chunks_text_rank;
    use crate::search::scope::ChunkMeta;
    use kio_search::rrf::BackendRank;
    use std::collections::BTreeMap;

    fn cited_by(image: &str, chunk: &str) -> ImageCandidate {
        ImageCandidate {
            image_hash: image.to_owned(),
            image_uri: format!("kio://s/object/image/{image}"),
            chunk_hash: chunk.to_owned(),
            meta: ChunkMeta {
                raw_hash: "sha256:r".to_owned(),
                tool_profile_hash: "sha256:p".to_owned(),
                r#gen: 0,
                heading_path: None,
                section_id: None,
                byte_start: 0,
                byte_end: 1,
                text: String::new(),
                unit_key: "doc:1".to_owned(),
            },
        }
    }
    let images = BTreeMap::from([
        ("img-a".to_owned(), cited_by("img-a", "c1")),
        ("img-b".to_owned(), cited_by("img-b", "c1")),
        // Cites a chunk the text lane never returned: no rank to inherit.
        ("img-z".to_owned(), cited_by("img-z", "c9")),
    ]);
    let mut text_ranks = vec![
        BackendRank {
            chunk_hash: "c1".to_owned(),
            rank: 1,
        },
        BackendRank {
            chunk_hash: "c2".to_owned(),
            rank: 2,
        },
    ];
    give_images_their_chunks_text_rank(&mut text_ranks, &images);
    assert_eq!(
        text_ranks
            .iter()
            .map(|rank| (rank.chunk_hash.as_str(), rank.rank))
            .collect::<Vec<_>>(),
        vec![("c1", 1), ("img-a", 1), ("img-b", 1), ("c2", 2)],
        "each image sits next to its citing chunk, carrying that chunk's rank"
    );

    // No images, or no text lane at all: byte-identical passthrough.
    let mut untouched = vec![BackendRank {
        chunk_hash: "c1".to_owned(),
        rank: 1,
    }];
    give_images_their_chunks_text_rank(&mut untouched, &BTreeMap::new());
    assert_eq!(untouched.len(), 1);
    let mut empty = Vec::new();
    give_images_their_chunks_text_rank(&mut empty, &images);
    assert!(empty.is_empty());
}

#[test]
fn r23_markdown_charge_and_send_share_one_verified_input() {
    use crate::management::ExplicitRoot;
    use crate::markdownize::execution::OnlineMarkdownizePrecondition;
    use crate::markdownize::execution::classify_online_markdownize_precondition;
    use crate::markdownize::execution::prorated_markdownize_cost;
    use crate::task_execution::estimate_online_markdownize_cost;
    use kio_pipeline::prepare::hash_bytes;
    use kio_pipeline::task::{TaskDescriptor, TaskStatus, TaskType};

    let root = tempfile::tempdir().unwrap();
    let repo = match crate::management::initialize_explicit_root(
        root.path(),
        &kio_core::control::ControlHandle::default(),
    )
    .unwrap()
    {
        ExplicitRoot::Created(repo) | ExplicitRoot::Existing(repo) => repo,
    };
    let pdf = b"%PDF-1.4\n1 0 obj << /Type /Pages /Kids [2 0 R] /Count 1 >> endobj\n2 0 obj << /Type /Page /Parent 1 0 R >> stream\nBT (verified billing input) Tj ET\nendstream endobj\n%%EOF\n";
    std::fs::write(root.path().join("doc.pdf"), pdf).unwrap();
    let task = TaskDescriptor {
        task_id: "task_verified_charge".to_owned(),
        task_type: TaskType::Markdownize,
        mode: None,
        input_path: "doc.pdf".to_owned(),
        input_hash: hash_bytes(pdf),
        previous_raw_hash: None,
        parent_run_id: None,
        changed_unit_keys: Vec::new(),
        output_ref: "online:mistral_ocr_markdownize".to_owned(),
        unit_keys: None,
        status: TaskStatus::Pending,
        attempts: 0,
        next_retry_at: None,
        deadline: None,
        heartbeat_at: None,
        fallback_reason: Some("ready_for_online_adapter".to_owned()),
        created_at: "2026-07-12T00:00:00Z".to_owned(),
        bbox_annotation_enabled: Some(true),
        hold_reason: None,
        reserved_usd: None,
        reserved_month: None,
        reservation_id: None,
    };

    let prepared = match classify_online_markdownize_precondition(&repo, &task) {
        OnlineMarkdownizePrecondition::Send(prepared) => prepared,
        _ => panic!("valid text-bearing PDF must reach the prepared send state"),
    };
    let valid_unit_key = prepared.prepared_units[0].unit_key.clone();
    for unit_keys in [
        Vec::new(),
        vec!["unknown-unit".to_owned()],
        vec![valid_unit_key.clone(), valid_unit_key.clone()],
    ] {
        let mut invalid = task.clone();
        invalid.unit_keys = Some(unit_keys);
        assert!(matches!(
            classify_online_markdownize_precondition(&repo, &invalid),
            OnlineMarkdownizePrecondition::Retire
        ));
    }
    let mut valid_retry = task.clone();
    valid_retry.unit_keys = Some(vec![valid_unit_key]);
    assert!(matches!(
        classify_online_markdownize_precondition(&repo, &valid_retry),
        OnlineMarkdownizePrecondition::Send(_)
    ));
    std::fs::write(root.path().join("doc.pdf"), b"").unwrap();

    assert_eq!(prepared.bytes, pdf);
    assert!(!prepared.prepared_units.is_empty());
    let charged = prorated_markdownize_cost(
        &task,
        prepared.bytes.len() as u64,
        prepared.prepared_units.len(),
        true,
    );
    assert_eq!(
        charged,
        estimate_online_markdownize_cost(pdf.len() as u64, true)
    );
    assert_eq!(
        estimate_online_markdownize_cost(pdf.len() as u64, true),
        estimate_online_markdownize_cost(pdf.len() as u64, false) * 1.25
    );
    assert!(
        charged > 0.0,
        "a post-check pathname change cannot mint a zero charge"
    );
}

#[test]
fn r12_1_read_search_tuning_parses_documented_keys() {
    use crate::search::query::read_search_tuning;
    use kio_search::query::DiversifyStrategy;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "[search.rrf]\nk = 5\nw_text = 0.0\nw_vector = 2.0\ncandidate_depth = 7\n\
             [search.diversify]\nenabled = true\nstrategy = \"off\"\nmax_per_raw_hash = 1\n",
    )
    .unwrap();
    let tuning = read_search_tuning(&path).unwrap();
    assert_eq!(tuning.rrf_k, Some(5));
    assert_eq!(tuning.rrf_w_text, Some(0.0));
    assert_eq!(tuning.rrf_w_vector, Some(2.0));
    assert_eq!(tuning.rrf_candidate_depth, Some(7));
    assert_eq!(tuning.div_enabled, Some(true));
    assert_eq!(tuning.div_strategy, Some(DiversifyStrategy::Off));
    assert_eq!(tuning.div_max_per_raw_hash, Some(1));
    // Absent file -> every field None (falls back to defaults at resolution).
    let empty = read_search_tuning(&dir.path().join("nope.toml")).unwrap();
    assert!(empty.rrf_k.is_none() && empty.div_strategy.is_none());
}

#[test]
fn r12_1_read_incremental_tuning_parses_documented_keys() {
    use crate::settings::read_incremental_tuning;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "[markdownize.incremental]\nenabled = false\nthreshold = 0.5\nmax_consecutive = 9\n",
    )
    .unwrap();
    let (enabled, threshold, max_consecutive) = read_incremental_tuning(&path).unwrap();
    assert_eq!(enabled, Some(false));
    assert_eq!(threshold, Some(0.5));
    assert_eq!(max_consecutive, Some(9));
    // Absent file -> all None.
    let (e, t, m) = read_incremental_tuning(&dir.path().join("nope.toml")).unwrap();
    assert!(e.is_none() && t.is_none() && m.is_none());
}

#[test]
fn ct4_bbox_001_reads_default_and_scope_over_user_annotation_policy() {
    use crate::settings::effective_bbox_annotation_policy;
    use crate::settings::read_bbox_annotation_config;

    let dir = tempfile::tempdir().unwrap();
    let scope = dir.path().join("scope.toml");
    let user = dir.path().join("user.toml");
    assert!(effective_bbox_annotation_policy(&scope, &user).unwrap());
    assert_eq!(read_bbox_annotation_config(&scope).unwrap(), None);

    std::fs::write(&user, "[markdownize]\nbbox_annotation = false\n").unwrap();
    assert_eq!(read_bbox_annotation_config(&user).unwrap(), Some(false));
    assert!(!effective_bbox_annotation_policy(&scope, &user).unwrap());

    std::fs::write(&scope, "[markdownize]\nbbox_annotation = true\n").unwrap();
    assert!(effective_bbox_annotation_policy(&scope, &user).unwrap());
}

#[test]
fn ct4_bbox_001_config_reads_are_bounded_regular_and_fail_closed() {
    use crate::settings::BBOX_ANNOTATION_CONFIG_MAX_BYTES;
    use crate::settings::effective_bbox_annotation_policy;
    use crate::settings::read_bbox_annotation_config;

    let dir = tempfile::tempdir().unwrap();

    let oversized = dir.path().join("oversized.toml");
    let file = std::fs::File::create(&oversized).unwrap();
    file.set_len(BBOX_ANNOTATION_CONFIG_MAX_BYTES + 1).unwrap();
    let error = read_bbox_annotation_config(&oversized).unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-STORE-OBJECT-OVERSIZED-001");

    let directory = dir.path().join("directory.toml");
    std::fs::create_dir(&directory).unwrap();
    let error = read_bbox_annotation_config(&directory).unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");

    let invalid_utf8 = dir.path().join("invalid-utf8.toml");
    std::fs::write(&invalid_utf8, [0xff]).unwrap();
    let error = read_bbox_annotation_config(&invalid_utf8).unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-CONFIG-SCHEMA-001");

    let invalid_toml = dir.path().join("invalid.toml");
    std::fs::write(&invalid_toml, "[").unwrap();
    let error = read_bbox_annotation_config(&invalid_toml).unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-CONFIG-SCHEMA-001");

    let valid_user = dir.path().join("valid-user.toml");
    std::fs::write(&valid_user, "[markdownize]\nbbox_annotation = false\n").unwrap();
    assert!(effective_bbox_annotation_policy(&invalid_toml, &valid_user).is_err());
}

#[cfg(unix)]
#[cfg(unix)]
#[test]
fn ct4_bbox_001_config_read_rejects_symlink() {
    use std::os::unix::fs::symlink;

    use crate::settings::read_bbox_annotation_config;

    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target.toml");
    let link = dir.path().join("link.toml");
    std::fs::write(&target, "[markdownize]\nbbox_annotation = true\n").unwrap();
    symlink(&target, &link).unwrap();

    let error = read_bbox_annotation_config(&link).unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
}

#[test]
fn r10_1_vector_capacity_message_classifier() {
    use crate::search::scope::is_vector_capacity_message;
    // R10-1(a): sqlite-vec's KNN k-ceiling and SQLite's bound-variable ceiling are
    // capacity limits (degrade the scope), not schema faults.
    assert!(is_vector_capacity_message(
        "index sqlite error: k value in knn query too large, provided 4200 and the limit is 4096"
    ));
    assert!(is_vector_capacity_message(
        "index sqlite error: too many SQL variables"
    ));
    // A genuine schema/contract error is NOT a capacity limit.
    assert!(!is_vector_capacity_message(
        "index schema error: no such column"
    ));
    assert!(!is_vector_capacity_message("some other failure"));
}

#[test]
fn pc8_long_query_still_builds_a_match_expression() {
    use crate::search::query::build_query_plan;
    // PC8 (05 §1.3 L110-115): the new MATCH-generation architecture has no
    // OLD-tier keyword cap (that was a pre-PC8 hardening specific to the
    // replaced bilingual/thousands-separator/tiered design) — 200
    // distinct ASCII tokens (>= 3 chars) still all become individually
    // quoted, `OR`-joined phrases with no token dropped.
    let query = (0..200)
        .map(|i| format!("kw{i:04}"))
        .collect::<Vec<_>>()
        .join(" ");
    let plan = build_query_plan(&query);
    let match_expr = plan.match_expr.expect("all tokens are >= 3 chars");
    assert_eq!(match_expr.matches(" OR ").count() + 1, 200);
    assert!(plan.short_tokens.is_empty());
}

#[test]
fn pc8_pc9_mixed_script_query_quotes_each_whitespace_token_once() {
    use crate::search::query::build_query_plan;
    // PC9 (05 §1.3 L113-115): tokenization splits on Unicode whitespace
    // only — a CJK run with no internal spaces is ONE token (not
    // decomposed into trigrams by this layer; FTS5's own trigram
    // tokenizer performs the substring match when this whole quoted
    // phrase is used as a MATCH operand, 04 §4.1). PC8: no cross-token
    // contamination — each whitespace-delimited piece is its own phrase.
    // 2026-07-22 feedback #2: each token's own script-boundary pieces
    // (`segment_script_runs`) additionally OR-join alongside it.
    let plan = build_query_plan("RAG パイプラインで再ランクに Merlin を使う 5 段構成の資料");
    let match_expr = plan.match_expr.expect("several tokens are >= 3 chars");
    // The original whitespace tokens are still present as their own
    // exact-phrase units.
    assert!(match_expr.contains("\"RAG\""));
    assert!(match_expr.contains("\"Merlin\""));
    assert!(match_expr.contains("\"パイプラインで再ランクに\""));
    assert!(match_expr.contains("\"段構成の資料\""));
    // "を使う" is exactly 3 Unicode scalars ("を","使","う") — PC12's
    // ">= 3" threshold puts it in the MATCH expression, not dropped as
    // short. It stays as a single unit: segmenting hiragana/han/hiragana
    // gives 3 sub-3-char pieces ("を","使","う"), none of which changes
    // the outcome, but the ORIGINAL token itself is what qualifies here.
    assert!(match_expr.contains("\"を使う\""));
    // Script-boundary segmentation of "パイプラインで再ランクに"
    // (katakana「パイプライン」/ hiragana「で」/ han「再」/
    // katakana「ランク」/ hiragana「に」) additionally OR-joins its own
    // >= 3 char pieces alongside the original token.
    assert!(
        match_expr.contains("\"パイプライン\""),
        "the katakana run must also be OR-joined as its own unit: {match_expr}"
    );
    assert!(
        match_expr.contains("\"ランク\""),
        "the second katakana run must also be OR-joined as its own unit: {match_expr}"
    );
    // Script-boundary segmentation of "段構成の資料" (han「段構成」/
    // hiragana「の」/ han「資料」) OR-joins its own >= 3 char piece too.
    assert!(
        match_expr.contains("\"段構成\""),
        "the leading han run must also be OR-joined as its own unit: {match_expr}"
    );
    // "5" is the only ORIGINAL token under 3 Unicode scalars here, but
    // this is a MIXED query (several long units exist) — 2026-07-22
    // feedback #2 drops every short unit outright rather than carrying
    // it as an AND-eligibility filter, so short_tokens comes back empty
    // (not `vec!["5"]`, the pre-feedback-#2 value).
    assert!(plan.short_tokens.is_empty());
    assert!(!match_expr.contains('5'));
}

#[test]
fn feedback2_segment_script_runs_splits_at_script_class_boundaries() {
    use crate::search::query::segment_script_runs;
    // 05 §1.3 L120-134 (2026-07-22 spec feedback #2), worked examples.

    // han run + hiragana run + katakana run (ー continues the preceding
    // katakana run rather than forcing its own split).
    assert_eq!(
        segment_script_runs("認証仕様のトークン"),
        vec!["認証仕様", "の", "トークン"]
    );
    // katakana run (ー continues it) + hiragana run.
    assert_eq!(segment_script_runs("スコープが"), vec!["スコープ", "が"]);
    // A run entirely inside one script class, with TWO internal ー, never
    // splits — "長音1片".
    assert_eq!(
        segment_script_runs("ローテーション"),
        vec!["ローテーション"]
    );
    // Digit-neighbor '.'/',' stay inside the Alnum run — each one whole
    // unit ("1 片").
    assert_eq!(segment_script_runs("3.2GB"), vec!["3.2GB"]);
    assert_eq!(segment_script_runs("99.9"), vec!["99.9"]);
    assert_eq!(segment_script_runs("3,600"), vec!["3,600"]);
    // '/' is `Other` — a pure separator, dropped, splitting the
    // surrounding Alnum runs.
    assert_eq!(
        segment_script_runs("read/write/admin"),
        vec!["read", "write", "admin"]
    );
    // `+` is also `Other` — "C" is the only Alnum run in "C++" (the
    // trailing "++" is dropped as a separator run). The ORIGINAL token
    // "C++" itself still becomes a unit in `build_query_plan` (below) —
    // segmentation never removes the token's own exact-phrase form.
    assert_eq!(segment_script_runs("C++"), vec!["C".to_owned()]);

    // `build_query_plan` keeps the ORIGINAL token "C++" as its own unit
    // (it is 3 Unicode scalars, so it is itself long and enters
    // `match_expr` verbatim) even though its only segmented piece "C" is
    // short and gets dropped (mixed-query short-unit rule, tested
    // separately below).
    let plan = crate::search::query::build_query_plan("C++");
    let match_expr = plan.match_expr.expect("\"C++\" is 3 Unicode scalars");
    assert_eq!(match_expr, "\"C++\"");
    assert!(plan.short_tokens.is_empty());
}

#[test]
fn feedback2_segment_script_runs_edge_cases() {
    use crate::search::query::segment_script_runs;
    // A `.`/`,` NOT flanked by digits on both sides is `Other` (dropped),
    // not folded into the neighboring Alnum run.
    assert_eq!(segment_script_runs("a.b"), vec!["a", "b"]);
    assert_eq!(segment_script_runs(".5"), vec!["5".to_owned()]);
    assert_eq!(segment_script_runs("5."), vec!["5".to_owned()]);
    assert_eq!(segment_script_runs(","), Vec::<String>::new());
    // ー at the very start of a token (no preceding character at all)
    // defaults to Katakana, per spec ("先頭含む").
    assert_eq!(segment_script_runs("ーテスト"), vec!["ーテスト"]);
    // Empty input yields no units.
    assert_eq!(segment_script_runs(""), Vec::<String>::new());
    // 々 (U+3005) and 〆 (U+3006) are Han, joining an adjacent Han run.
    assert_eq!(segment_script_runs("時々"), vec!["時々".to_owned()]);
}

#[test]
fn r23_06_katakana_middle_dot_is_other_not_katakana() {
    use crate::search::query::segment_script_runs;
    // 05 §1.3 2026-07-22 spec feedback #2 correction: U+30FB (・,
    // katakana middle dot) is script=Common, not script=Katakana,
    // despite sitting inside the katakana Unicode block -- classified
    // `Other` (a separator) so it splits the surrounding katakana runs
    // instead of gluing them into one unit that never matches either
    // half.
    assert_eq!(
        segment_script_runs("アクセス・トークン"),
        vec!["アクセス", "トークン"]
    );
    // ー (U+30FC, the existing long-vowel special case) is unaffected --
    // still continues a preceding katakana run on either side of ・.
    assert_eq!(
        segment_script_runs("キー・バリュー"),
        vec!["キー", "バリュー"]
    );
    // ヽ/ヾ (U+30FD/FE, katakana iteration marks) and ヶ (U+30F6) remain
    // Katakana and do NOT split their run -- only U+30FB changes class.
    assert_eq!(
        segment_script_runs("アヶヽヾイ"),
        vec!["アヶヽヾイ".to_owned()]
    );
}

#[test]
fn feedback2_mixed_query_drops_short_units_entirely_not_and_filtered() {
    use crate::search::query::build_query_plan;
    // 05 §1.3 2026-07-22 feedback #2 / step4b-contract-tests-p2c.md
    // R-追記: a mixed query (>= 1 long unit) fully discards every short
    // unit — it never enters `match_expr`, and (unlike the superseded
    // PC12/13 AND-instr eligibility predicate) never enters
    // `short_tokens` either.
    let plan = build_query_plan("authentication AI");
    let match_expr = plan.match_expr.expect("\"authentication\" is long");
    assert!(match_expr.contains("\"authentication\""));
    assert!(
        !match_expr.contains("\"AI\""),
        "a short unit must never enter the MATCH expression: {match_expr}"
    );
    assert!(
        plan.short_tokens.is_empty(),
        "a mixed query's short units must be fully discarded, not carried \
             as an AND-eligibility filter: {:?}",
        plan.short_tokens
    );

    // A short unit produced BY SEGMENTATION (not an original whitespace
    // token) is dropped the same way: "スコープが" splits into long
    // "スコープ" + short "が" — "が" must not surface anywhere.
    let plan = build_query_plan("スコープが read/write/admin");
    let match_expr = plan.match_expr.expect("several long units exist");
    assert!(match_expr.contains("\"スコープ\""));
    assert!(match_expr.contains("\"read\""));
    assert!(
        !match_expr.contains("\"が\""),
        "a segmentation-derived short unit must never enter the MATCH \
             expression: {match_expr}"
    );
    assert!(plan.short_tokens.is_empty());
}

#[test]
fn feedback2_pure_short_query_preserves_pre_feedback2_behavior() {
    use crate::search::query::build_query_plan;
    // 05 §1.3 2026-07-22 feedback #2: when EVERY unit is short (which,
    // since no segmented piece can be longer than its own token, is
    // equivalent to "every original token is short"), `match_expr` stays
    // `None` and `short_tokens` is the RAW token list — unchanged from
    // the pre-feedback-#2 behavior (PC11/PC14's bounded LIKE fallback),
    // except for R23-28's first-occurrence dedup (below).
    let plan = build_query_plan("AI 認");
    assert!(plan.match_expr.is_none());
    assert_eq!(plan.short_tokens, vec!["AI".to_owned(), "認".to_owned()]);

    // A single short CJK token (PC11's own worked example).
    let plan = build_query_plan("認証");
    assert!(plan.match_expr.is_none());
    assert_eq!(plan.short_tokens, vec!["認証".to_owned()]);
}

/// R23-28 (05 §1.3 L129-131 "全 unit 集合から重複を除く"): a pure-short
/// query's `short_tokens` is now first-occurrence deduplicated — a
/// regression the pre-fix `partition` never applied to this branch (only
/// the long-unit `units` list deduped). Undeduplicated, a query
/// repeating one short token thousands of times fed one SQLite bind
/// parameter per repetition to `short_token_instr_sql`/
/// `short_token_bind_values`, risking the bound-variable limit.
#[test]
fn r23_28_pure_short_query_dedups_short_tokens_first_occurrence() {
    use crate::search::query::build_query_plan;
    let plan = build_query_plan("ai ai");
    assert!(plan.match_expr.is_none());
    assert_eq!(plan.short_tokens, vec!["ai".to_owned()]);

    // First-occurrence order is preserved (PC14's ORDER BY tie-break
    // keys on the FIRST token, so it must survive dedup unchanged) even
    // when a later-occurring token repeats first.
    let plan = build_query_plan("ai to ai to in");
    assert!(plan.match_expr.is_none());
    assert_eq!(
        plan.short_tokens,
        vec!["ai".to_owned(), "to".to_owned(), "in".to_owned()]
    );

    // A pathological many-repeat query collapses to one bind value
    // instead of thousands.
    let many = "ai ".repeat(40_000);
    let plan = build_query_plan(many.trim());
    assert!(plan.match_expr.is_none());
    assert_eq!(plan.short_tokens, vec!["ai".to_owned()]);
}

#[test]
fn pc8_deterministic_equivalence_expansion_is_query_derived() {
    use crate::search::query::build_query_plan;
    // 05 §1.3 L116-123 (2026-07-22 spec feedback #1): PC8's original
    // "query 由来でない追加語を含まない" reading was too strict — it
    // banned a token's own deterministic numeral/dictionary equivalence
    // form along with genuinely GUESSED words (synonyms, history,
    // context). Both are now restored as OR-injected forms; eval
    // M3-2/M3-3 (09 §4.3's Recall@10 >= 0.8 gate) measured 13/14
    // failures tracing to exactly this gap before the spec fix.
    let plan = build_query_plan("chunk size was 512 tokens in the retrieval pipeline doc");
    let match_expr = plan.match_expr.expect("several tokens are >= 3 chars");
    assert!(
        match_expr.contains("\"チャンク\""),
        "chunk -> チャンク must be OR-injected: {match_expr}"
    );
    assert!(
        match_expr.contains("\"トークン\""),
        "tokens -> トークン must be OR-injected: {match_expr}"
    );
    assert!(
        match_expr.contains("\"パイプライン\""),
        "pipeline -> パイプライン must be OR-injected: {match_expr}"
    );
    assert!(match_expr.contains("\"chunk\""));
    assert!(match_expr.contains("\"pipeline\""));

    // Plain digits gain their thousands-grouped twin.
    let plan = build_query_plan("TTL is 3600 seconds");
    let match_expr = plan.match_expr.expect("several tokens are >= 3 chars");
    assert!(match_expr.contains("\"3600\""));
    assert!(
        match_expr.contains("\"3,600\""),
        "a >= 4 digit numeral must gain its thousands-separated twin: {match_expr}"
    );

    // Bidirectional: a well-formed comma-grouped numeral loses its commas too.
    let plan = build_query_plan("total is 3,600 units");
    let match_expr = plan.match_expr.expect("several tokens are >= 3 chars");
    assert!(
        match_expr.contains("\"3600\""),
        "a grouped numeral must gain its plain twin: {match_expr}"
    );
}

#[test]
fn pc8_numeric_equivalent_form_is_bidirectional_and_bounded() {
    use crate::search::query::numeric_equivalent_form;
    // (a) plain digits >= 4 gain the grouped twin.
    assert_eq!(numeric_equivalent_form("3600").as_deref(), Some("3,600"));
    assert_eq!(numeric_equivalent_form("30000").as_deref(), Some("30,000"));
    assert_eq!(numeric_equivalent_form("4096").as_deref(), Some("4,096"));
    assert_eq!(
        numeric_equivalent_form("1234567").as_deref(),
        Some("1,234,567")
    );
    // (b) reverse: a well-formed grouped numeral loses its commas.
    assert_eq!(numeric_equivalent_form("3,600").as_deref(), Some("3600"));
    assert_eq!(numeric_equivalent_form("30,000").as_deref(), Some("30000"));
    assert_eq!(
        numeric_equivalent_form("1,234,567").as_deref(),
        Some("1234567")
    );
    // (c) below the 4-digit floor, real thousands grouping never
    // applies (no comma in "999") — no equivalence form either direction.
    assert_eq!(numeric_equivalent_form("999"), None);
    assert_eq!(numeric_equivalent_form("12"), None);
    // (d) not a numeral at all, or a malformed grouping (not exactly
    // 3-digit trailing groups) — left untouched, never reinterpreted.
    assert_eq!(numeric_equivalent_form("abcd"), None);
    assert_eq!(numeric_equivalent_form("12,3"), None);
    assert_eq!(numeric_equivalent_form("1,23"), None);
    assert_eq!(numeric_equivalent_form(",600"), None);
    assert_eq!(numeric_equivalent_form("600,"), None);
    assert_eq!(numeric_equivalent_form("3,6000"), None);
}

#[test]
fn lane_override_rejects_asking_for_both_lanes() {
    assert!(
        resolve_invocation_lane(
            LaneOverride::new(true, true),
            None,
            &kio_core::control::ControlHandle::default()
        )
        .is_err()
    );
}

fn lane_test_row(kind: kio_pipeline::ledger::RequestKind) -> kio_pipeline::ledger::BatchRequestRow {
    kio_pipeline::ledger::BatchRequestRow {
        key: kio_pipeline::ledger::model::TaskKey {
            scope_id: "scope".to_owned(),
            adapter_kind: "markdownize".to_owned(),
            input_hash: "sha256:input".to_owned(),
            tool_profile_hash: "sha256:tool".to_owned(),
        },
        state: kio_pipeline::ledger::model::BatchState::Intent,
        request_kind: kind,
        intent_token: Some("tok".to_owned()),
        upload_id: None,
        batch_job_id: None,
        provider_scope_id: None,
        job_create_started_at: None,
        stale_after_at: None,
        submission_seq: 0,
        attempts: 0,
        contract_violation_count: 0,
        estimated_usd: 0.0,
        error: None,
        completed_at: None,
        created_at: 0,
    }
}

fn stored_chunk_line(rowid: u64, id: &str) -> String {
    let mut chunk = ledger_test_chunk();
    chunk.rowid = rowid;
    chunk.association_rowid = rowid;
    chunk.row.unit_key = id.to_owned();
    chunk.row.chunking_config_hash = kio_core::cas::hash_bytes(id.as_bytes());
    chunk.row.text = id.to_owned();
    chunk.row.text_hash = kio_core::cas::hash_bytes(id.as_bytes());
    chunk.row.byte_end = id.len() as u64;
    chunk.row.chunk_id = kio_index::chunking::chunk_hash(&chunk.row).unwrap();
    serde_json::to_string(&chunk).unwrap()
}

#[test]
fn realtime_moves_a_new_ocr_send_to_the_sync_lane() {
    // Default (Batch lane) is unchanged...
    assert_eq!(
        markdownize_send_lane(true, None, false),
        MarkdownizeSendLane::Batch
    );
    // ...and `--realtime` opts into the sync lane's double rate.
    assert_eq!(
        markdownize_send_lane(true, None, true),
        MarkdownizeSendLane::Sync
    );
}

#[test]
fn realtime_never_hijacks_an_in_flight_batch_row() {
    // An open batch row owns a live provider job; switching it to the sync
    // lane mid-flight would strand that job outside §5.8 recovery.
    let row = lane_test_row(kio_pipeline::ledger::RequestKind::Batch);
    assert_eq!(
        markdownize_send_lane(true, Some(&row), true),
        MarkdownizeSendLane::Batch
    );
    // Without a batch client the row still defers to recovery, not to sync.
    assert_eq!(
        markdownize_send_lane(false, Some(&row), true),
        MarkdownizeSendLane::DeferToRecovery
    );
}

#[test]
fn reopened_sync_row_is_deferred_and_never_reused_for_a_second_send() {
    let row = lane_test_row(kio_pipeline::ledger::RequestKind::Sync);
    assert_eq!(
        markdownize_send_lane(true, Some(&row), false),
        MarkdownizeSendLane::DeferToRecovery
    );
    assert_eq!(
        markdownize_send_lane(false, Some(&row), true),
        MarkdownizeSendLane::DeferToRecovery
    );
}

#[test]
fn realtime_leaves_the_no_batch_client_case_on_sync() {
    assert_eq!(
        markdownize_send_lane(false, None, false),
        MarkdownizeSendLane::Sync
    );
    assert_eq!(
        markdownize_send_lane(false, None, true),
        MarkdownizeSendLane::Sync
    );
}

#[test]
fn the_invocation_lane_defaults_to_batch() {
    // No `--realtime` was recorded by these unit tests, so the default the
    // ruling fixes (Batch = half price) is what both adapters would take.
    assert!(!realtime_lane_requested());
    assert_eq!(effective_invocation_lane(), PreferredRequestKind::Batch);
}

#[test]
fn embedding_token_estimate_counts_cjk_at_one_token_per_char() {
    // 4 Latin chars ≈ 1 token; 4 CJK scalars ≈ 4 tokens. The old
    // `chars / 4` rule charged the Japanese case at a quarter of its cost.
    assert!((estimate_embedding_tokens("abcd") - 1.0).abs() < 1e-9);
    assert!((estimate_embedding_tokens("日本語だ") - 4.0).abs() < 1e-9);
    // Mixed text adds the two ratios rather than picking one.
    assert!((estimate_embedding_tokens("abcd日本語だ") - 5.0).abs() < 1e-9);
    assert!(estimate_embedding_tokens("").abs() < 1e-9);
}

#[test]
fn embedding_rate_is_lane_aware_and_batch_is_half_of_sync() {
    let adapter_settings =
        &std::sync::Arc::new(kio_adapter::tool_lock::AdapterRuntimeSettings::default());

    // Published `gemini-embedding-2` text rates (07 §5.3 の 2026-07-24 訂正):
    // $0.20 / 1M sync, $0.10 / 1M batch.
    let sync = embedding_usd_per_token(adapter_settings, PreferredRequestKind::Sync);
    let batch = embedding_usd_per_token(adapter_settings, PreferredRequestKind::Batch);
    assert!((sync - 0.000_000_2).abs() < 1e-15, "sync rate: {sync}");
    assert!((batch - 0.000_000_1).abs() < 1e-15, "batch rate: {batch}");
    assert!((sync - batch * 2.0).abs() < 1e-15);
}

/// R24 (5 系統一致): a DECLARED `tools.toml` price must still split by lane.
/// The pre-R24 code returned the declared value before consulting `lane`,
/// so the moment a user declared `[embedding.*.pricing] tokens_in` — which
/// 07 §5.3 calls the normative source — Batch and Sync collapsed to one
/// price and every batch job was recorded at double what it cost.
#[test]
fn a_declared_price_is_the_standard_rate_and_batch_is_still_half() {
    // An arbitrary declared rate, deliberately not the published one.
    let declared = 0.000_000_5;
    let sync = lane_rate(declared, PreferredRequestKind::Sync);
    let batch = lane_rate(declared, PreferredRequestKind::Batch);
    assert!(
        (sync - declared).abs() < 1e-15,
        "sync must be the declared rate: {sync}"
    );
    assert!(
        (batch - declared / 2.0).abs() < 1e-15,
        "batch must be half the declared rate, not equal to it: {batch}"
    );
}

/// The lane split is a property of the LANE, not of the embedding role.
/// Both priced roles bill 2:1 (OCR $4 vs $2 per 1,000 pages, embedding
/// $0.20 vs $0.10 per 1M tokens), and `markdownize` settlement used to read
/// its declaration raw at both sites — right only because that declaration
/// happened to hold the batch rate, which made a `--realtime` OCR run
/// settle at half the provider's charge.
#[test]
fn the_lane_split_applies_to_a_page_price_the_same_way() {
    let declared_pages = 0.004;
    assert!((lane_rate(declared_pages, PreferredRequestKind::Sync) - 0.004).abs() < 1e-15);
    assert!((lane_rate(declared_pages, PreferredRequestKind::Batch) - 0.002).abs() < 1e-15);
}

/// An under-record is the direction a budget cap cannot defend against: the
/// cap only ever sees the too-small number, so it releases spending that
/// the provider has already billed for. Pin the ordering explicitly.
#[test]
fn a_batch_rate_is_never_above_the_sync_rate_for_any_declared_price() {
    for declared in [0.0, 1e-9, 0.002, 0.004, 7.5] {
        let sync = lane_rate(declared, PreferredRequestKind::Sync);
        let batch = lane_rate(declared, PreferredRequestKind::Batch);
        assert!(batch <= sync, "batch {batch} must not exceed sync {sync}");
    }
}

#[test]
fn embedding_cost_multiplies_the_token_estimate_by_the_lane_rate() {
    let adapter_settings =
        &std::sync::Arc::new(kio_adapter::tool_lock::AdapterRuntimeSettings::default());

    // 1M Latin chars ≈ 250k tokens → $0.05 at the sync rate.
    let text = "a".repeat(1_000_000);
    let usd = estimate_embedding_cost(adapter_settings, &text, PreferredRequestKind::Sync);
    assert!((usd - 0.05).abs() < 1e-9, "usd: {usd}");
    // The same text on the Batch lane costs exactly half.
    let batch_usd = estimate_embedding_cost(adapter_settings, &text, PreferredRequestKind::Batch);
    assert!((usd - batch_usd * 2.0).abs() < 1e-12);
}

#[test]
fn query_embedding_is_always_priced_on_the_sync_lane() {
    // A search cannot wait out a batch job's turnaround, so the query lane
    // must never be priced at the batch rate even once the enrichment
    // driver moves to Batch.
    assert_eq!(query_embedding_send_lane(), PreferredRequestKind::Sync);
}

#[test]
fn r11_2_exit_override_priority_batch_vs_enrichment() {
    use crate::task_execution::ExecOutcome;
    use crate::task_execution::batch_exit_override;
    use crate::task_execution::enrichment_exit_override;
    use kio_core::ExitCode;

    // A clean pass overrides nothing (exit 0).
    assert!(batch_exit_override(&ExecOutcome::default()).is_none());
    assert!(enrichment_exit_override(&ExecOutcome::default()).is_none());

    // Batch priority: auth (5) > budget-paused (6) > some-retryable (3) >
    // all-permanent (4).
    let all = ExecOutcome {
        executed: 1,
        failed: 3,
        paused: 1,
        auth_failed: 1,
        failed_retryable: 1,
        inflight: 0,
        held_unreadable: 0,
    };
    assert_eq!(batch_exit_override(&all), Some(ExitCode::AuthError));
    assert_eq!(
        batch_exit_override(&ExecOutcome {
            auth_failed: 0,
            ..all
        }),
        Some(ExitCode::BudgetExceeded)
    );
    assert_eq!(
        batch_exit_override(&ExecOutcome {
            auth_failed: 0,
            paused: 0,
            ..all
        }),
        Some(ExitCode::PartialFailure)
    );
    // Failures present but none retryable / auth / paused → all permanent (4).
    assert_eq!(
        batch_exit_override(&ExecOutcome {
            failed: 2,
            ..ExecOutcome::default()
        }),
        Some(ExitCode::PermanentFailure)
    );

    // Enrichment (index/repair/reindex) overrides ONLY on auth (5) / budget (6);
    // a retryable/permanent embedding failure stays exit 0 (disclosed in JSON).
    assert_eq!(
        enrichment_exit_override(&ExecOutcome {
            auth_failed: 1,
            ..ExecOutcome::default()
        }),
        Some(ExitCode::AuthError)
    );
    assert_eq!(
        enrichment_exit_override(&ExecOutcome {
            paused: 1,
            ..ExecOutcome::default()
        }),
        Some(ExitCode::BudgetExceeded)
    );
    assert!(
        enrichment_exit_override(&ExecOutcome {
            failed: 2,
            failed_retryable: 2,
            ..ExecOutcome::default()
        })
        .is_none()
    );
}

#[test]
fn qb15_parent_and_child_index_exits_keep_user_action_priority() {
    use crate::invocation::merged_index_exit_override;
    use kio_core::ExitCode;

    assert_eq!(
        merged_index_exit_override(None, [], false),
        None,
        "a clean parent and child set exits successfully"
    );
    assert_eq!(
        merged_index_exit_override(None, [], true),
        Some(ExitCode::PartialFailure)
    );
    assert_eq!(
        merged_index_exit_override(
            Some(ExitCode::PartialFailure),
            [ExitCode::BudgetExceeded, ExitCode::AuthError],
            true,
        ),
        Some(ExitCode::AuthError)
    );
    assert_eq!(
        merged_index_exit_override(
            Some(ExitCode::PartialFailure),
            [ExitCode::BudgetExceeded],
            false,
        ),
        Some(ExitCode::BudgetExceeded)
    );
    assert_eq!(
        merged_index_exit_override(None, [ExitCode::PermanentFailure], false),
        Some(ExitCode::PartialFailure),
        "a failed child cannot erase already-published parent work"
    );
    assert_eq!(
        merged_index_exit_override(None, [ExitCode::AuthError], true),
        Some(ExitCode::AuthError),
        "a Result::Err child retains the same auth priority as an output marker"
    );
}

#[test]
fn r10_4_partial_retry_plan_gates_on_retryability_and_budget() {
    use crate::batch::command::partial_retry_plan_from_manifest;
    use kio_pipeline::markdownize::NormalizedInstanceManifest;
    use kio_pipeline::prepare::unit_ref;
    let manifest = |first_error: Option<&str>, second_error: Option<&str>| {
        serde_json::from_value::<NormalizedInstanceManifest>(serde_json::json!({
                "raw_hash": format!("sha256:{}", "a".repeat(64)),
                "tool_profile_hash": format!("sha256:{}", "b".repeat(64)),
                "gen": 0,
                "parent_gen": null,
                "run_id": "run_x",
                "units": [
                    {"order":0,"unit_key":"page:1","unit_ref":unit_ref("page:1"),"unit_type":"page","status":if first_error.is_some() { "failed" } else { "done" },"prepared_hash":format!("sha256:{}", "c".repeat(64)),"preparation_profile_hash":format!("sha256:{}", "9".repeat(64)),"error_kind":first_error,"unit_object_hash":first_error.is_none().then(|| format!("sha256:{}", "e".repeat(64)))},
                    {"order":1,"unit_key":"page:2","unit_ref":unit_ref("page:2"),"unit_type":"page","status":if second_error.is_some() { "failed" } else { "done" },"prepared_hash":format!("sha256:{}", "d".repeat(64)),"preparation_profile_hash":format!("sha256:{}", "9".repeat(64)),"error_kind":second_error,"unit_object_hash":second_error.is_none().then(|| format!("sha256:{}", "f".repeat(64)))},
                ],
                "generated_at": "2026-07-05T00:00:00Z",
            }))
            .unwrap()
    };

    // A Failed unit with a RETRYABLE kind is re-enqueued, with a finite budget.
    let plan = partial_retry_plan_from_manifest(&manifest(None, Some("network_error")), 0);
    assert_eq!(plan.retryable_units, vec!["page:2".to_owned()]);
    assert_eq!(plan.max_attempts, Some(5));

    // A Failed unit with a NON-retryable (permanent) kind is never re-enqueued.
    let plan = partial_retry_plan_from_manifest(&manifest(None, Some("invalid_input")), 0);
    assert!(plan.retryable_units.is_empty());

    // Mixed: QA45 (step4b-contract-tests-p3a.md §M, 04 §5.3 L738-740) made
    // contract_violation retryable (max_attempts=1, same-mode retry for
    // output jitter) instead of dropped — both units now survive, and
    // the plan's ceiling is the min across them (1, tighter than
    // network_error's 5).
    let plan = partial_retry_plan_from_manifest(
        &manifest(Some("contract_violation"), Some("network_error")),
        0,
    );
    assert_eq!(
        plan.retryable_units,
        vec!["page:1".to_owned(), "page:2".to_owned()]
    );
    assert_eq!(plan.max_attempts, Some(1));

    // A rate-limit unit is explicitly unlimited, but it must not keep an
    // exhausted NetworkError sibling eligible forever through the shared
    // task counter.
    let plan =
        partial_retry_plan_from_manifest(&manifest(Some("network_error"), Some("rate_limit")), 5);
    assert_eq!(plan.retryable_units, vec!["page:2".to_owned()]);
    assert_eq!(plan.max_attempts, None);
}

#[test]
fn subset_retry_preserves_unrequested_failed_unit_kind() {
    use std::collections::BTreeSet;

    use kio_adapter::types::{FailedUnit, MarkdownizeMode, MarkdownizeResponse};

    use crate::batch::command::partial_retry_plan_from_manifest;
    use crate::markdownize::execution::PreviousInstance;
    use crate::markdownize::execution::merged_failed_unit_retry_kinds;
    use kio_pipeline::markdownize::NormalizedInstanceManifest;
    use kio_pipeline::prepare::unit_ref;
    use kio_pipeline::task::RetryErrorKind;
    let manifest: NormalizedInstanceManifest = serde_json::from_value(serde_json::json!({
            "raw_hash": format!("sha256:{}", "a".repeat(64)),
            "tool_profile_hash": format!("sha256:{}", "b".repeat(64)),
            "gen": 0, "parent_gen": null, "run_id": "prior",
            "units": [
                {"order":0,"unit_key":"page:1","unit_ref":unit_ref("page:1"),"unit_type":"page","status":"failed","prepared_hash":format!("sha256:{}", "c".repeat(64)),"preparation_profile_hash":format!("sha256:{}", "9".repeat(64)),"unit_object_hash":null,"error_kind":"invalid_input"},
                {"order":1,"unit_key":"page:2","unit_ref":unit_ref("page:2"),"unit_type":"page","status":"failed","prepared_hash":format!("sha256:{}", "d".repeat(64)),"preparation_profile_hash":format!("sha256:{}", "9".repeat(64)),"unit_object_hash":null,"error_kind":"network_error"}
            ], "generated_at":"2026-09-08T00:00:00Z"
        })).unwrap();
    let previous = PreviousInstance {
        manifest,
        units: Vec::new(),
        prepared_units: Vec::new(),
    };
    let response = MarkdownizeResponse {
        mode_used: MarkdownizeMode::Full,
        updated_units: Vec::new(),
        unchanged_unit_keys: Vec::new(),
        added_units: Vec::new(),
        removed_unit_keys: Vec::new(),
        fallback_to_full: false,
        reason: None,
        usage: None,
        failed_units: vec![FailedUnit {
            unit_key: "page:2".to_owned(),
            error_kind: "network_error".to_owned(),
        }],
    };
    let kinds = merged_failed_unit_retry_kinds(
        &response,
        Some(&previous),
        Some(&BTreeSet::from(["page:2".to_owned()])),
    );
    assert_eq!(kinds.get("page:1"), Some(&RetryErrorKind::InvalidInput));
    assert_eq!(kinds.get("page:2"), Some(&RetryErrorKind::NetworkError));
    assert!(
        partial_retry_plan_from_manifest(&previous.manifest, 0)
            .retryable_units
            .contains(&"page:2".to_owned())
    );
}

#[test]
fn r10_5_persist_failure_is_retryable() {
    use crate::markdownize::execution::persist_failure_retry_kind;
    use kio_pipeline::task::retry_policy;
    // R10-5: a post-OCR persist I/O fault must be retryable so `batch retry` can
    // recover the already-billed normalized output.
    assert!(retry_policy(persist_failure_retry_kind()).retryable);
}

#[test]
fn r10_6_open_cache_path_uses_full_hash() {
    use crate::open_view::open_cache_path;
    // R10-6: the per-object cache dir is the FULL 64-hex sha256, not a 12-char
    // prefix, so a prefix+basename collision can't serve the wrong object.
    let hash = format!("sha256:{}", "a".repeat(64));
    let path = open_cache_path(
        "raw",
        "doc.md",
        &hash,
        &kio_core::control::ControlHandle::default(),
    );
    let dir = path
        .parent()
        .unwrap()
        .file_name()
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(dir.len(), 64, "cache dir must be the full 64-hex hash");
    assert_eq!(dir, "a".repeat(64));
    let leaf = path.file_name().unwrap().to_str().unwrap();
    assert_ne!(leaf, "doc.md");
    assert!(leaf.starts_with("open-"));
    assert!(leaf.ends_with(".md"));
}

#[test]
fn r10_6_open_cache_write_is_atomic_readonly_and_cleans_temp() {
    use crate::open_view::write_open_cache_atomic;
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("obj").join("doc.md");
    std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
    write_open_cache_atomic(&cache, b"evidence bytes").unwrap();
    assert_eq!(std::fs::read(&cache).unwrap(), b"evidence bytes");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&cache).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o400, "published cache file must be read-only 0400");
    }
    let stray: Vec<_> = std::fs::read_dir(cache.parent().unwrap())
        .unwrap()
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(".tmp-"))
        .collect();
    assert!(stray.is_empty(), "no temp may remain: {stray:?}");
}

#[test]
fn r10_6_open_cache_torn_write_leaves_no_temp() {
    use crate::open_view::write_open_cache_atomic;
    // Force the rename to fail (destination is an existing directory) — a torn
    // write must clean its temp and never publish a partial file.
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("doc.md");
    std::fs::create_dir(&cache).unwrap();
    let result = write_open_cache_atomic(&cache, b"bytes");
    assert!(result.is_err(), "write onto a directory must fail");
    let stray: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(".tmp-"))
        .collect();
    assert!(
        stray.is_empty(),
        "torn write must clean its temp: {stray:?}"
    );
}

#[test]
fn r10_8_snapshot_tree_entries_insert_is_atomic() {
    use crate::search::query::TreeEntryProjection;
    use crate::search::query::insert_snapshot_tree_entries;
    use rusqlite::Connection;
    let conn = Connection::open_in_memory().unwrap();
    // A CHECK lets the test force a mid-batch failure deterministically.
    conn.execute_batch(
        "CREATE TABLE tree_entries (
                 commit_hash TEXT NOT NULL,
                 path TEXT NOT NULL,
                 raw_hash TEXT NOT NULL CHECK(raw_hash <> 'BAD'),
                 tool_profile_hash TEXT,
                 gen INTEGER,
                 manifest_hash TEXT,
                 PRIMARY KEY (commit_hash, path));",
    )
    .unwrap();
    let count = |conn: &Connection| -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM tree_entries WHERE commit_hash='c1'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };
    // R10-8: a failing 2nd row rolls the 1st row back — the projection stays
    // all-or-nothing so `existing` remains 0 and the next read reprojects.
    let torn = vec![
        TreeEntryProjection {
            path: "a.md".to_owned(),
            raw_hash: "sha256:aa".to_owned(),
            tool_profile_hash: Some("sha256:tool".to_owned()),
            r#gen: Some(0),
            manifest_hash: Some("sha256:manifest".to_owned()),
        },
        TreeEntryProjection {
            path: "b.md".to_owned(),
            raw_hash: "BAD".to_owned(),
            tool_profile_hash: Some("sha256:tool".to_owned()),
            r#gen: Some(0),
            manifest_hash: Some("sha256:manifest".to_owned()),
        },
    ];
    assert!(insert_snapshot_tree_entries(&conn, "c1", &torn).is_err());
    assert_eq!(count(&conn), 0, "partial inserts must be rolled back");
    // A clean batch commits every row atomically.
    let good = vec![
        TreeEntryProjection {
            path: "a.md".to_owned(),
            raw_hash: "sha256:aa".to_owned(),
            tool_profile_hash: Some("sha256:tool".to_owned()),
            r#gen: Some(0),
            manifest_hash: Some("sha256:manifest".to_owned()),
        },
        TreeEntryProjection {
            path: "b.md".to_owned(),
            raw_hash: "sha256:bb".to_owned(),
            tool_profile_hash: None,
            r#gen: Some(1),
            manifest_hash: Some("sha256:manifest2".to_owned()),
        },
    ];
    insert_snapshot_tree_entries(&conn, "c1", &good).unwrap();
    assert_eq!(count(&conn), 2);
}

#[test]
fn r9_8_atomic_write_cas_object_removes_temp_on_failure() {
    use crate::object_persistence::atomic_write_cas_object;
    // R9-8: a torn derived-CAS write must not leave an orphan `.tmp-*` in the
    // fanout dir. Force the rename to fail deterministically by making the
    // destination an existing directory after the temp is created + fsynced.
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("obj");
    std::fs::create_dir(&dest).unwrap();
    let result = atomic_write_cas_object(dir.path(), &dest, b"derived-bytes");
    assert!(result.is_err(), "write onto a directory must fail");
    let stray: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".tmp-"))
        .collect();
    assert!(
        stray.is_empty(),
        "temp not cleaned up on failure: {stray:?}"
    );
}

#[test]
fn r23_cand_031_existing_prepared_slot_must_match_exact_bytes() {
    use crate::object_persistence::atomic_write_cas_object;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("prepared-object");
    std::fs::write(&path, b"poison").unwrap();
    let error = atomic_write_cas_object(dir.path(), &path, b"secure").unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
    assert_eq!(std::fs::read(&path).unwrap(), b"poison");
}

#[cfg(unix)]
#[cfg(unix)]
#[test]
fn r23_prepared_writer_rejects_symlinked_store_ancestor() {
    use std::os::unix::fs::symlink;

    use crate::object_persistence::write_prepared_objects;
    use kio_core::scope::Repository;
    use kio_pipeline::prepare::{PreparedUnit, UnitFingerprint, UnitType, hash_bytes};

    let root = tempfile::tempdir().unwrap();
    let repo = Repository::init(root.path()).unwrap();
    let outside = tempfile::tempdir().unwrap();
    let prepared_root = repo.kio_dir().join("objects/prepared");
    if prepared_root.exists() {
        std::fs::remove_dir_all(&prepared_root).unwrap();
    }
    std::fs::create_dir_all(prepared_root.parent().unwrap()).unwrap();
    symlink(outside.path(), &prepared_root).unwrap();
    let bytes = b"outside-write";
    let prepared_hash = hash_bytes(bytes);
    let unit = PreparedUnit {
        order: 0,
        unit_key: "file:0".to_owned(),
        unit_type: UnitType::File,
        prepared_hash: prepared_hash.clone(),
        preparation_profile_hash: "sha256:test-prepare-profile".to_owned(),
        fingerprint: UnitFingerprint {
            perceptual_hash: prepared_hash.clone(),
            text_hash: prepared_hash.clone(),
            visual_hash: prepared_hash.clone(),
        },
        mime: Some("text/plain".to_owned()),
        page_number: None,
    };

    let error =
        write_prepared_objects(&repo, &[unit], &[prepared_hash], bytes, "text/plain").unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
    assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none());
}

#[test]
fn r9_5_unit_file_and_orphan_temp_classification() {
    use crate::object_persistence::is_normalized_unit_file;
    use crate::object_persistence::is_orphan_temp_name;
    // Real unit files: 16 lowercase-hex chars + `.json`.
    assert!(is_normalized_unit_file("1a2b3c4d5e6f7089.json"));
    assert!(is_normalized_unit_file("0000000000000000.json"));
    // Not unit files — must be skipped by copy_normalized_instance_gen.
    assert!(!is_normalized_unit_file("manifest.json"));
    assert!(!is_normalized_unit_file(".DS_Store"));
    assert!(!is_normalized_unit_file(".tmp-99999-0000abcd"));
    assert!(!is_normalized_unit_file("1a2b3c4d5e6f7089.md")); // wrong ext
    assert!(!is_normalized_unit_file("1A2B3C4D5E6F7089.json")); // uppercase hex
    assert!(!is_normalized_unit_file("1a2b.json")); // too short
    assert!(!is_normalized_unit_file("1a2b3c4d5e6f7089z.json")); // 17 chars / non-hex
    // Orphan-temp detection (GC'd on reindex).
    assert!(is_orphan_temp_name(".tmp-99999-0000abcd"));
    assert!(is_orphan_temp_name(".1a2b3c4d5e6f7089.json.tmp-123-456"));
    assert!(!is_orphan_temp_name(".DS_Store"));
    assert!(!is_orphan_temp_name("manifest.json"));
}

#[test]
fn mock_embedding_vector_is_deterministic_and_normalized() {
    let a = deterministic_embedding_vector("認証仕様 トークン", 768);
    let b = deterministic_embedding_vector("認証仕様 トークン", 768);
    assert_eq!(a.len(), 768);
    assert_eq!(a, b, "same seed must reproduce the same vector");
    let norm = a.iter().map(|value| value * value).sum::<f32>().sqrt();
    assert!((norm - 1.0).abs() < 1e-4, "vector must be L2-normalized");
    let other = deterministic_embedding_vector("別のクエリ", 768);
    assert_ne!(a, other, "different seeds must differ");
}

// CL56-58: two serial task charges must re-read the ledger inside their own
// `BEGIN IMMEDIATE` Tx (`reserve_or_reuse_task_charge`), so the second sees the
// first's reservation and is denied when it would exceed the cap — the cap is
// never breached even without concurrency, and nothing is double-reserved.
#[test]
fn serial_task_charges_reread_and_enforce_the_cap() {
    use crate::ledger_access::TaskChargeOutcome;
    use crate::ledger_access::budget_cap_config;
    use crate::ledger_access::reserve_or_reuse_task_charge;
    use crate::ledger_access::task_ledger_key;
    use kio_pipeline::budget::BudgetCaps;
    use kio_pipeline::ledger::RequestKind;
    use std::collections::BTreeMap;

    let tmp = crate::test_support::PrivateTempDir::new();
    let ledger =
        kio_pipeline::ledger::LedgerDb::initialize(tmp.path().join("device/cost-ledger.sqlite"))
            .unwrap();
    let caps = BudgetCaps {
        device_monthly_usd_cap: 1.0,
        folder_monthly_usd_cap: None,
        device_per_adapter: BTreeMap::new(),
        hard_stop: true,
        warn_at_percent: 80,
    };
    let cap_config = budget_cap_config(&caps, "scope", "embedding");

    // First charge: 0.6 <= 1.0 remaining → Reserved.
    let first_key = task_ledger_key("scope", "embedding", "sha256:first", "profile");
    let first = reserve_or_reuse_task_charge(
        &crate::invocation::Invocation {
            context: &crate::context::test_support::context(std::path::Path::new(".")),
            settings: &std::sync::Arc::new(
                kio_adapter::tool_lock::AdapterRuntimeSettings::default(),
            ),
            control: ledger.control(),
            locks: &kio_core::scope::LockSession::default(),
        },
        &ledger,
        &first_key,
        0.6,
        &cap_config,
        false,
        RequestKind::Sync,
    )
    .unwrap();
    assert!(matches!(first, TaskChargeOutcome::Reserved { .. }));

    // Second charge re-reads inside its own Tx: spent=0.6, remaining=0.4 < 0.6 →
    // BudgetExceeded (serial charges cannot exceed the cap even without
    // concurrent writers).
    let second_key = task_ledger_key("scope", "embedding", "sha256:second", "profile");
    let second = reserve_or_reuse_task_charge(
        &crate::invocation::Invocation {
            context: &crate::context::test_support::context(std::path::Path::new(".")),
            settings: &std::sync::Arc::new(
                kio_adapter::tool_lock::AdapterRuntimeSettings::default(),
            ),
            control: ledger.control(),
            locks: &kio_core::scope::LockSession::default(),
        },
        &ledger,
        &second_key,
        0.6,
        &cap_config,
        false,
        RequestKind::Sync,
    )
    .unwrap();
    assert!(matches!(second, TaskChargeOutcome::BudgetExceeded));

    // `override_budget` bypasses the same denial (docs/04 §5.4 `kio batch resume
    // --override-budget`).
    let overridden = reserve_or_reuse_task_charge(
        &crate::invocation::Invocation {
            context: &crate::context::test_support::context(std::path::Path::new(".")),
            settings: &std::sync::Arc::new(
                kio_adapter::tool_lock::AdapterRuntimeSettings::default(),
            ),
            control: ledger.control(),
            locks: &kio_core::scope::LockSession::default(),
        },
        &ledger,
        &second_key,
        0.6,
        &cap_config,
        true,
        RequestKind::Sync,
    )
    .unwrap();
    assert!(matches!(overridden, TaskChargeOutcome::Reserved { .. }));
}

#[test]
fn store_lock_preserves_retryable_cli_error_contract() {
    use crate::object_persistence::pipeline_to_kio;
    use kio_core::ExitCode;

    let err = pipeline_to_kio(kio_pipeline::PipelineError::locked(
        "/tmp/example-device-lock.lock",
    ));
    assert_eq!(err.error_code(), "KIO-E-STORE-LOCKED-001");
    assert_eq!(err.exit_code(), ExitCode::PartialFailure);
}

// `release_task_charge_if_open` (the retired JSONL design's
// `settle_task_reservation` twin) must be a safe, idempotent no-op when no
// ledger row exists for the key at all — a forged/stale task-side stamp (or
// simply a task that was never charged) can never manufacture a settlement.
#[test]
fn release_task_charge_if_open_is_a_no_op_without_a_matching_row() {
    use crate::ledger_access::release_task_charge_if_open;
    use crate::ledger_access::task_ledger_key;

    let tmp = crate::test_support::PrivateTempDir::new();
    let ledger =
        kio_pipeline::ledger::LedgerDb::initialize(tmp.path().join("device/cost-ledger.sqlite"))
            .unwrap();
    let key = task_ledger_key("scope", "markdownize", "sha256:never-charged", "profile");
    assert!(!release_task_charge_if_open(&ledger, &key).unwrap());
}

#[test]
fn q1_read_stored_chunks_tolerates_torn_tail() {
    use crate::index::chunk_ledger::chunks_jsonl_path;
    use crate::index::chunk_ledger::read_stored_chunks;
    let dir = tempfile::tempdir().unwrap();
    let kio_dir = dir.path().join(".kio");
    let path = chunks_jsonl_path(&kio_dir);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    // Two intact rows, then a torn final line (crash / ENOSPC mid-write_all,
    // no trailing newline).
    let mut contents = format!(
        "{}\n{}\n",
        stored_chunk_line(1, "c1"),
        stored_chunk_line(2, "c2")
    );
    contents.push_str(r#"{"rowid":3,"chunk_id":"c3","raw_hash":"sha256:"#);
    std::fs::write(&path, contents).unwrap();
    let chunks = read_stored_chunks(&kio_dir).unwrap();
    assert_eq!(
        chunks.len(),
        2,
        "torn tail must be dropped so index/repair self-heal"
    );
}

#[test]
fn q1_read_stored_chunks_flags_mid_file_corruption() {
    use crate::index::chunk_ledger::chunks_jsonl_path;
    use crate::index::chunk_ledger::read_stored_chunks;
    let dir = tempfile::tempdir().unwrap();
    let kio_dir = dir.path().join(".kio");
    let path = chunks_jsonl_path(&kio_dir);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    // A broken NON-final line cannot be a torn tail -> STORE-CORRUPT (exit 4),
    // not the old CONFIG-SCHEMA (exit 2) misclassification.
    let contents = format!(
        "{}\n{}\n{}\n",
        stored_chunk_line(1, "c1"),
        r#"{"rowid":2,"chunk_id":BROKEN"#,
        stored_chunk_line(3, "c3")
    );
    std::fs::write(&path, contents).unwrap();
    let err = read_stored_chunks(&kio_dir).unwrap_err();
    assert_eq!(err.error_code(), "KIO-E-STORE-CORRUPT-001");
}

#[test]
fn ct4_chunk_ledger_rejects_missing_association_rowid() {
    use crate::index::chunk_ledger::chunks_jsonl_path;
    use crate::index::chunk_ledger::read_stored_chunks;
    let dir = tempfile::tempdir().unwrap();
    let kio_dir = dir.path().join(".kio");
    let path = chunks_jsonl_path(&kio_dir);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut first: serde_json::Value = serde_json::from_str(&stored_chunk_line(42, "c42")).unwrap();
    first.as_object_mut().unwrap().remove("association_rowid");
    std::fs::write(&path, format!("{first}\n")).unwrap();

    let err = read_stored_chunks(&kio_dir).unwrap_err();
    assert_eq!(err.error_code(), "KIO-E-STORE-CORRUPT-001");
}

#[test]
fn ct4_chunk_ledger_retains_two_configs_for_one_chunk() {
    use crate::index::chunk_ledger::chunks_jsonl_path;
    use crate::index::chunk_ledger::read_stored_chunks;
    let dir = tempfile::tempdir().unwrap();
    let kio_dir = dir.path().join(".kio");
    let path = chunks_jsonl_path(&kio_dir);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut first: serde_json::Value =
        serde_json::from_str(&stored_chunk_line(7, "shared")).unwrap();
    first["association_rowid"] = serde_json::json!(11);
    let mut second = first.clone();
    second["association_rowid"] = serde_json::json!(29);
    second["chunking_config_hash"] = serde_json::json!(format!("sha256:{}", "9".repeat(64)));
    let mut second_chunk: StoredChunk = serde_json::from_value(second.clone()).unwrap();
    second_chunk.row.chunk_id = kio_index::chunking::chunk_hash(&second_chunk.row).unwrap();
    second = serde_json::to_value(second_chunk).unwrap();
    std::fs::write(&path, format!("{first}\n{second}\n")).unwrap();

    let chunks = read_stored_chunks(&kio_dir).unwrap();
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].rowid, 7);
    assert_eq!(chunks[1].rowid, 7);
    assert_eq!(chunks[0].association_rowid, 11);
    assert_eq!(chunks[1].association_rowid, 29);
    assert_ne!(
        chunks[0].row.chunking_config_hash,
        chunks[1].row.chunking_config_hash
    );
}

#[test]
fn q2_open_cas_byte_object_rejects_corrupt_object() {
    use crate::open_view::cas_object_path;
    use crate::open_view::open_cas_byte_object;
    use crate::search::scope::ScopeTarget;
    use kio_pipeline::prepare::hash_bytes;
    let dir = tempfile::tempdir().unwrap();
    let kio_dir = dir.path().join(".kio");
    // The correct `sha256:` filename for the AUTHENTIC bytes...
    let hash = hash_bytes(b"authentic prepared object");
    let object_path = cas_object_path(&kio_dir, "prepared", &hash).unwrap();
    std::fs::create_dir_all(object_path.parent().unwrap()).unwrap();
    // ...but the file under it holds corrupt/torn bytes (non-atomic write).
    std::fs::write(&object_path, b"corrupt partial bytes").unwrap();
    let target = ScopeTarget {
        repo_root: dir.path().to_path_buf(),
        kio_dir,
        scope_id: "01H00000000000000000000000".to_owned(),
    };
    let err = open_cas_byte_object(
        &target,
        "prepared",
        false,
        &hash,
        None,
        &kio_core::control::ControlHandle::default(),
    )
    .unwrap_err();
    assert_eq!(
        err.error_code(),
        "KIO-E-STORE-CORRUPT-001",
        "a corrupt CAS object must not be served as authentic evidence"
    );
}

#[test]
fn portable_cas_leaf_uses_digest_and_presence_rejects_nonregular_slot() {
    use crate::open_view::cas_object_path;
    use crate::open_view::cas_object_present;
    use kio_core::cas::MAX_RAW_OBJECT_BYTES;
    use kio_pipeline::prepare::hash_bytes;

    let dir = tempfile::tempdir().unwrap();
    let hash = hash_bytes(b"portable object");
    let path = cas_object_path(dir.path(), "raw", &hash).unwrap();
    assert_eq!(
        path.file_name().and_then(|name| name.to_str()),
        hash.strip_prefix("sha256:")
    );
    assert!(!path.to_string_lossy().contains("sha256:"));

    std::fs::create_dir_all(&path).unwrap();
    let error = cas_object_present(dir.path(), "raw", &hash, MAX_RAW_OBJECT_BYTES).unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
}

#[cfg(not(windows))]
fn non_current_colon_leaf(
    kio_dir: &std::path::Path,
    subdir: &str,
    hash: &str,
) -> std::path::PathBuf {
    let digest = hash.strip_prefix("sha256:").unwrap();
    kio_dir
        .join("objects")
        .join(subdir)
        .join(&digest[0..2])
        .join(&digest[2..4])
        .join(hash)
}

#[cfg(not(windows))]
#[cfg(not(windows))]
#[test]
fn non_current_only_cas_leaf_is_ignored_as_not_found() {
    use crate::open_view::cas_object_present;
    use crate::open_view::read_cas_byte_object;
    use kio_core::cas::MAX_RAW_OBJECT_BYTES;
    use kio_pipeline::prepare::hash_bytes;

    let dir = tempfile::tempdir().unwrap();
    let bytes = b"non-current object layout";
    let hash = hash_bytes(bytes);
    let non_current = non_current_colon_leaf(dir.path(), "prepared", &hash);
    std::fs::create_dir_all(non_current.parent().unwrap()).unwrap();
    std::fs::write(non_current, bytes).unwrap();

    assert!(!cas_object_present(dir.path(), "prepared", &hash, MAX_RAW_OBJECT_BYTES).unwrap());
    assert!(
        read_cas_byte_object(dir.path(), "prepared", &hash, MAX_RAW_OBJECT_BYTES)
            .unwrap()
            .is_none()
    );
}

#[test]
fn canonical_cas_read_succeeds_and_hash_mismatch_fails_closed() {
    use crate::open_view::cas_object_path;
    use crate::open_view::read_cas_byte_object;
    use kio_core::cas::MAX_RAW_OBJECT_BYTES;
    use kio_pipeline::prepare::hash_bytes;

    let dir = tempfile::tempdir().unwrap();
    let bytes = b"canonical object";
    let hash = hash_bytes(bytes);
    let canonical = cas_object_path(dir.path(), "prepared", &hash).unwrap();
    std::fs::create_dir_all(canonical.parent().unwrap()).unwrap();
    std::fs::write(&canonical, bytes).unwrap();
    let (resolved, loaded) =
        read_cas_byte_object(dir.path(), "prepared", &hash, MAX_RAW_OBJECT_BYTES)
            .unwrap()
            .unwrap();
    assert_eq!(resolved, canonical);
    assert_eq!(loaded, bytes);

    std::fs::write(&canonical, b"hash mismatch").unwrap();
    let error =
        read_cas_byte_object(dir.path(), "prepared", &hash, MAX_RAW_OBJECT_BYTES).unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
}

#[test]
fn prepared_writer_publishes_new_objects_to_canonical_leaf() {
    use crate::object_persistence::write_canonical_cas_object_or_reuse;
    use crate::open_view::cas_object_path;
    use kio_pipeline::prepare::hash_bytes;

    let dir = tempfile::tempdir().unwrap();
    let kio_dir = dir.path().join(".kio");
    std::fs::create_dir(&kio_dir).unwrap();
    let bytes = b"new prepared bytes";
    let hash = hash_bytes(bytes);
    write_canonical_cas_object_or_reuse(&kio_dir, "prepared", &hash, bytes).unwrap();
    let canonical = cas_object_path(&kio_dir, "prepared", &hash).unwrap();
    assert_eq!(std::fs::read(canonical).unwrap(), bytes);
}

#[cfg(not(windows))]
#[cfg(not(windows))]
#[test]
fn non_current_conflict_cannot_influence_canonical_read_or_write() {
    use crate::object_persistence::write_canonical_cas_object_or_reuse;
    use crate::open_view::cas_object_path;
    use crate::open_view::read_cas_byte_object;
    use kio_core::cas::MAX_RAW_OBJECT_BYTES;
    use kio_pipeline::prepare::hash_bytes;

    let dir = tempfile::tempdir().unwrap();
    let kio_dir = dir.path().join(".kio");
    std::fs::create_dir(&kio_dir).unwrap();
    let bytes = b"canonical prepared bytes";
    let hash = hash_bytes(bytes);
    let non_current = non_current_colon_leaf(&kio_dir, "prepared", &hash);
    std::fs::create_dir_all(non_current.parent().unwrap()).unwrap();
    std::fs::write(&non_current, b"conflicting non-current bytes").unwrap();

    write_canonical_cas_object_or_reuse(&kio_dir, "prepared", &hash, bytes).unwrap();
    let canonical = cas_object_path(&kio_dir, "prepared", &hash).unwrap();
    let (resolved, loaded) =
        read_cas_byte_object(&kio_dir, "prepared", &hash, MAX_RAW_OBJECT_BYTES)
            .unwrap()
            .unwrap();
    assert_eq!(resolved, canonical);
    assert_eq!(loaded, bytes);
    assert_eq!(
        std::fs::read(non_current).unwrap(),
        b"conflicting non-current bytes"
    );
}

#[test]
fn q3_reclaims_orphaned_running_task_to_pending() {
    use crate::batch::command::reclaim_orphaned_running_tasks;
    use kio_pipeline::markdownize::MarkdownizeMode;
    use kio_pipeline::task::{TaskDescriptor, TaskStatus, TaskStore, TaskType};
    let dir = tempfile::tempdir().unwrap();
    let store = TaskStore::new(dir.path());
    let task = TaskDescriptor {
        task_id: "t1".to_owned(),
        task_type: TaskType::Markdownize,
        mode: Some(MarkdownizeMode::Full),
        input_path: "notes.txt".to_owned(),
        input_hash: format!("sha256:{}", "a".repeat(64)),
        previous_raw_hash: None,
        parent_run_id: None,
        changed_unit_keys: Vec::new(),
        output_ref: crate::index::pipeline::online_output_ref("test_markdownize_adapter"),
        unit_keys: None,
        // A task stuck Running is bit-identical to a crash between the
        // Running-persist and the Done-persist.
        status: TaskStatus::Running,
        attempts: 0,
        next_retry_at: None,
        deadline: None,
        heartbeat_at: Some("2020-01-01T00:00:00Z".to_owned()),
        fallback_reason: Some("ready_for_online_adapter".to_owned()),
        created_at: "2026-07-04T00:00:00Z".to_owned(),
        bbox_annotation_enabled: Some(true),
        hold_reason: None,
        reserved_usd: None,
        reserved_month: None,
        reservation_id: None,
    };
    store.append(&task).unwrap();
    let reclaimed = reclaim_orphaned_running_tasks(&store).unwrap();
    assert_eq!(reclaimed, 1, "the orphaned Running task must be reclaimed");
    let tasks = store.all().unwrap();
    assert_eq!(tasks[0].status, TaskStatus::Pending);
    assert!(
        tasks[0].heartbeat_at.is_none(),
        "stale heartbeat must be cleared on reclaim"
    );
}

#[test]
fn chunk_authentication_rejects_forged_text_with_a_valid_identity_hash() {
    use crate::index::rebuild::authenticate_chunk_row;
    use crate::index::rebuild::unit_authorities_from_inputs;
    use kio_index::chunking::ChunkingConfig;
    use kio_index::chunking::ChunkingInput;
    use kio_index::chunking::NormalizedUnitInput;
    use kio_index::chunking::chunk_normalized_instance;
    use kio_pipeline::prepare::hash_bytes;

    let markdown = "# Heading\nattested text\n".to_owned();
    let unit = NormalizedUnitInput {
        raw_hash: hash_bytes(b"raw"),
        tool_profile_hash: hash_bytes(b"profile"),
        r#gen: 3,
        unit_key: "document".to_owned(),
        unit_content_hash: hash_bytes(markdown.as_bytes()),
        markdown,
    };
    let authorities = unit_authorities_from_inputs(std::slice::from_ref(&unit));
    let mut rows = chunk_normalized_instance(ChunkingInput {
        raw_path: "doc.md".to_owned(),
        units: vec![unit],
        config: ChunkingConfig {
            chunking_config_hash: hash_bytes(b"config"),
            strategy: "heading".to_owned(),
            max_chars: 100,
        },
        created_at: "2026-08-12T00:00:00Z".to_owned(),
    })
    .unwrap();
    let mut forged = rows.pop().unwrap();
    forged.text = "forged text".to_owned();
    forged.text_hash = hash_bytes(forged.text.as_bytes());

    assert!(authenticate_chunk_row(&forged, &authorities).is_err());
}

#[test]
fn registry_snapshot_error_mapping_preserves_typed_exit_and_closed_reason() {
    let path = std::path::Path::new("/private/test/scope-registry.sqlite");
    let unsafe_error =
        crate::scope_access::registry_snapshot_unsafe_error(path, "linked main".to_owned());
    assert_eq!(
        unsafe_error.error_code(),
        "KIO-E-REGISTRY-SNAPSHOT-UNSAFE-001"
    );
    assert_eq!(
        unsafe_error.exit_code(),
        kio_core::ExitCode::PermanentFailure
    );
    assert_eq!(unsafe_error.context()["reason"], "unsafe_integrity");
    assert_eq!(
        unsafe_error.context()["registry_path"],
        path.display().to_string()
    );

    let transient_error = crate::scope_access::registry_snapshot_transient_error(
        path,
        "unstable_or_busy",
        "retry budget exhausted".to_owned(),
    );
    assert_eq!(transient_error.error_code(), "KIO-E-REGISTRY-SNAPSHOT-001");
    assert_eq!(
        transient_error.exit_code(),
        kio_core::ExitCode::PartialFailure
    );
    assert_eq!(transient_error.context()["reason"], "unstable_or_busy");

    let impossible_missing = crate::scope_access::registry_snapshot_lookup_error(
        path,
        kio_index::registry::RegistrySnapshotError::Missing,
    );
    assert_eq!(
        impossible_missing.error_code(),
        "KIO-E-REGISTRY-SNAPSHOT-UNSAFE-001"
    );
    assert_eq!(
        impossible_missing.exit_code(),
        kio_core::ExitCode::PermanentFailure
    );
    assert_eq!(impossible_missing.context()["reason"], "unsafe_integrity");
}

#[test]
fn ledger_snapshot_error_mapping_preserves_typed_exit_and_closed_reason() {
    let path = "/private/test/cost-ledger.sqlite";
    let unsafe_error =
        crate::object_persistence::pipeline_to_kio(kio_pipeline::PipelineError::ledger_snapshot(
            path,
            kio_pipeline::ledger::LedgerSnapshotError::UnsafeIntegrity(
                "linked ledger main".to_owned(),
            ),
        ));
    assert_eq!(
        unsafe_error.error_code(),
        "KIO-E-LEDGER-SNAPSHOT-UNSAFE-001"
    );
    assert_eq!(
        unsafe_error.exit_code(),
        kio_core::ExitCode::PermanentFailure
    );
    assert_eq!(unsafe_error.context()["reason"], "unsafe_integrity");
    assert_eq!(unsafe_error.context()["ledger_path"], path);

    let transient_error =
        crate::object_persistence::pipeline_to_kio(kio_pipeline::PipelineError::ledger_snapshot(
            path,
            kio_pipeline::ledger::LedgerSnapshotError::UnstableBusy(
                "retry budget exhausted".to_owned(),
            ),
        ));
    assert_eq!(transient_error.error_code(), "KIO-E-LEDGER-SNAPSHOT-001");
    assert_eq!(
        transient_error.exit_code(),
        kio_core::ExitCode::PartialFailure
    );
    assert_eq!(transient_error.context()["reason"], "unstable_or_busy");

    let impossible_missing =
        crate::object_persistence::pipeline_to_kio(kio_pipeline::PipelineError::ledger_snapshot(
            path,
            kio_pipeline::ledger::LedgerSnapshotError::Missing,
        ));
    assert_eq!(
        impossible_missing.error_code(),
        "KIO-E-LEDGER-SNAPSHOT-UNSAFE-001"
    );
    assert_eq!(
        impossible_missing.exit_code(),
        kio_core::ExitCode::PermanentFailure
    );
}

fn search_request(mode: Option<crate::commands::SearchMode>) -> crate::commands::SearchArgs {
    crate::commands::SearchArgs {
        query: Some("q".to_owned()),
        mode,
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
        online: false,
        offline: false,
    }
}

#[test]
fn constructed_search_request_preserves_mode_and_limit_validation() {
    let parsed = crate::search::query::parsed_search(search_request(Some(
        crate::commands::SearchMode::Vector,
    )))
    .unwrap();
    assert_eq!(parsed.requested_mode, kio_search::query::SearchMode::Vector);
    assert!(parsed.explicit_mode);
    let parsed = crate::search::query::parsed_search(search_request(None)).unwrap();
    assert_eq!(parsed.requested_mode, kio_search::query::SearchMode::Auto);
    assert!(!parsed.explicit_mode);
    let mut request = search_request(None);
    request.limit = 0;
    assert!(crate::search::query::parsed_search(request).is_err());
}

#[test]
fn constructed_search_request_rejects_invalid_time_scope_combination() {
    let mut request = search_request(None);
    request.at = Some("HEAD".to_owned());
    assert!(crate::search::query::parsed_search(request).is_err());
    let mut request = search_request(None);
    request.at = Some("HEAD".to_owned());
    request.scope = Some(std::path::PathBuf::from("."));
    assert!(crate::search::query::parsed_search(request).is_ok());
}

#[test]
fn current_policy_redacts_denied_alias_without_hiding_allowed_alias() {
    use crate::management::ExplicitRoot;
    use crate::management::binding_for_repo;
    use crate::management::initialize_explicit_root;
    use crate::search::scope::redact_current_policy_paths;
    use kio_core::management::read_record;
    use kio_pipeline::policy::CurrentPolicyEvaluator;

    let root = tempfile::tempdir().unwrap();
    let repo =
        match initialize_explicit_root(root.path(), &kio_core::control::ControlHandle::default())
            .unwrap()
        {
            ExplicitRoot::Created(repo) => repo,
            ExplicitRoot::Existing(_) => panic!("fresh root must be created"),
        };
    std::fs::write(root.path().join(".kioignore"), "denied.md\n").unwrap();
    let binding = binding_for_repo(&repo).unwrap();
    let policy =
        CurrentPolicyEvaluator::load(&binding, read_record(&binding).unwrap().case_insensitive)
            .unwrap();

    assert_eq!(
        redact_current_policy_paths(
            &policy,
            vec!["allowed.md".to_owned(), "denied.md".to_owned()],
        )
        .unwrap(),
        vec!["allowed.md"],
    );
    assert!(
        redact_current_policy_paths(&policy, vec!["denied.md".to_owned()])
            .unwrap()
            .is_empty()
    );
}

#[test]
fn image_payload_mime_is_derived_from_verified_magic_only() {
    assert_eq!(
        crate::embedding::enrichment::verified_image_mime(b"\x89PNG\r\n\x1a\nbody").unwrap(),
        "image/png"
    );
    assert_eq!(
        crate::embedding::enrichment::verified_image_mime(&[0xff, 0xd8, 0xff, 0x00]).unwrap(),
        "image/jpeg"
    );
    assert!(crate::embedding::enrichment::verified_image_mime(b"not-an-image").is_err());
}

#[test]
fn embedding_secret_hold_applies_to_authenticated_local_http() {
    use crate::embedding::enrichment::secret_embedding_hold_required;
    use kio_adapter::types::ExecutionMode;

    for mode in [ExecutionMode::OnlineApi, ExecutionMode::OfflineApi] {
        assert!(
            secret_embedding_hold_required(mode, false, true),
            "{mode:?} secret content must remain held without its paired grant"
        );
        assert!(!secret_embedding_hold_required(mode, true, true));
    }
    assert!(!secret_embedding_hold_required(
        ExecutionMode::DeterministicLibrary,
        false,
        true,
    ));
}

#[test]
fn embedding_admission_rechecks_policy_and_exact_network_grant() {
    let adapter_settings =
        &std::sync::Arc::new(kio_adapter::tool_lock::AdapterRuntimeSettings::default());

    use crate::egress::admit_embedding_transport;
    use crate::management::ExplicitRoot;
    use crate::management::initialize_explicit_root;
    use crate::search::scope::current_embedding_policy;
    use kio_adapter::catalog::{
        AdoptedEmbeddingExecution, DeclaredEmbeddingProfile, EmbeddingExecution,
    };

    let _environment_lock = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let device = crate::test_support::PrivateTempDir::new();
    let _config =
        kio_core::test_control::TestEnvGuard::set("XDG_CONFIG_HOME", device.path().join("config"));
    let _data = kio_core::test_control::TestEnvGuard::set("XDG_DATA_HOME", device.path());
    let _cache =
        kio_core::test_control::TestEnvGuard::set("XDG_CACHE_HOME", device.path().join("cache"));
    let root = tempfile::tempdir().unwrap();
    let repo =
        match initialize_explicit_root(root.path(), &kio_core::control::ControlHandle::default())
            .unwrap()
        {
            ExplicitRoot::Created(repo) => repo,
            ExplicitRoot::Existing(_) => panic!("fresh root"),
        };
    let profile = DeclaredEmbeddingProfile {
        tool_id: "gemini_embedding_2".to_owned(),
        dimensions: 768,
        distance: "cosine".to_owned(),
        modality: "multimodal".to_owned(),
        profile_hash: format!("sha256:{}", "a".repeat(64)),
    };
    let policy = current_embedding_policy(&repo).unwrap();
    let missing_grant = admit_embedding_transport(
        adapter_settings,
        &repo,
        EmbeddingExecution::Online(AdoptedEmbeddingExecution::Mock),
        &profile,
        &policy,
        false,
    )
    .unwrap_err();
    assert_eq!(
        missing_grant.error_code(),
        "KIO-E-ADAPTER-APPROVAL-REQUIRED-001"
    );

    // The prior selection cannot be used after current Ignore changes:
    // revalidation precedes the grant check at the actual transport edge.
    std::fs::write(root.path().join(".kioignore"), "later-denied.md\n").unwrap();
    let stale = admit_embedding_transport(
        adapter_settings,
        &repo,
        EmbeddingExecution::Online(AdoptedEmbeddingExecution::Mock),
        &profile,
        &policy,
        false,
    )
    .unwrap_err();
    assert_ne!(stale.error_code(), "KIO-E-ADAPTER-APPROVAL-REQUIRED-001");
    assert!(stale.to_string().contains("changed"), "{stale}");
}

#[test]
fn cursor_policy_revalidation_rejects_a_live_ignore_change() {
    use crate::management::ExplicitRoot;
    use crate::management::binding_for_repo;
    use crate::management::initialize_explicit_root;
    use crate::search::scope::SearchedScopeInfo;
    use crate::search::scope::revalidate_current_search_policies;
    use kio_core::management::read_record;
    use kio_pipeline::policy::CurrentPolicyEvaluator;
    use std::collections::BTreeMap;
    let root = tempfile::tempdir().unwrap();
    let repo =
        match initialize_explicit_root(root.path(), &kio_core::control::ControlHandle::default())
            .unwrap()
        {
            ExplicitRoot::Created(repo) => repo,
            ExplicitRoot::Existing(_) => panic!("fresh root"),
        };
    let binding = binding_for_repo(&repo).unwrap();
    let policy =
        CurrentPolicyEvaluator::load(&binding, read_record(&binding).unwrap().case_insensitive)
            .unwrap();
    let searched = [SearchedScopeInfo {
        scope_id: "s".to_owned(),
        scope_path: repo.kio_dir().to_path_buf(),
        repo_root: root.path().to_path_buf(),
        snapshot_at: "sha256:test".to_owned(),
        max_rowid: 0,
        max_association_rowid: 0,
        chunking_config_hash: "sha256:test".to_owned(),
        index_generation: "test".to_owned(),
        journal_active_at_prepare: false,
        shallow_skipped: 0,
        runtime_binding_filter: None,
    }];
    std::fs::write(root.path().join(".kioignore"), "later-denied.md\n").unwrap();
    let error = revalidate_current_search_policies(
        &searched,
        &BTreeMap::from([("s".to_owned(), policy)]),
        true,
    )
    .unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-SEARCH-CURSOR-001");
}

#[test]
fn only_typed_same_scope_image_ownership_selects_local_bytes_or_projection() {
    use crate::embedding::enrichment::authorized_local_image_object_hash;
    use crate::embedding::enrichment::local_image_object_hash;
    use crate::embedding::enrichment::referenced_authorized_image_hashes_for_chunks;
    use crate::embedding_owners::RetainedEmbeddingChunk;
    use crate::image_authority::{ImageAuthorityEntry, ImageAuthorityMap};

    let hash = format!("sha256:{}", "a".repeat(64));
    let foreign_uri = format!("kio://scope-b/object/image/{hash}");
    let local_uri = format!("kio://scope-a/object/image/{hash}");
    let foreign = format!("![foreign]({foreign_uri})");
    let local = format!("![unowned]({local_uri})");
    let chunks = vec![
        RetainedEmbeddingChunk {
            chunk_id: "foreign".to_owned(),
            text: foreign.into(),
            text_hash: "foreign".to_owned(),
            raw_path: "public.md".to_owned(),
            requires_secret_approval: false,
            is_head_owner: false,
        },
        RetainedEmbeddingChunk {
            chunk_id: "local".to_owned(),
            text: local.into(),
            text_hash: "local".to_owned(),
            raw_path: "public.md".to_owned(),
            requires_secret_approval: false,
            is_head_owner: false,
        },
    ];
    let unowned = ImageAuthorityMap::default();
    let owned = ImageAuthorityMap::test_map(BTreeMap::from([(
        hash.clone(),
        ImageAuthorityEntry {
            policy_allowed: true,
            requires_secret_approval: false,
        },
    )]));

    assert_eq!(local_image_object_hash("scope-a", &foreign_uri), None);
    assert_eq!(
        referenced_authorized_image_hashes_for_chunks(&chunks[..1], "scope-a", &owned, true),
        BTreeSet::new(),
        "a foreign URI must not select same-hash local CAS bytes"
    );
    assert_eq!(
        referenced_authorized_image_hashes_for_chunks(&chunks, "scope-a", &unowned, true),
        BTreeSet::new(),
        "same-scope Markdown alone cannot grant local image ownership"
    );
    assert_eq!(
        referenced_authorized_image_hashes_for_chunks(&chunks, "scope-a", &owned, true),
        BTreeSet::from([hash.clone()]),
        "a typed, allowed owner may create an image vector/projection association"
    );
    assert_eq!(
        authorized_local_image_object_hash("scope-a", &owned, &local_uri),
        Some(hash)
    );
}

#[test]
fn secret_typed_owner_cannot_be_bypassed_by_a_public_same_scope_link() {
    use crate::embedding::enrichment::referenced_authorized_image_hashes_for_chunks;
    use crate::embedding_owners::RetainedEmbeddingChunk;
    use crate::image_authority::{ImageAuthorityEntry, ImageAuthorityMap};

    let hash = format!("sha256:{}", "b".repeat(64));
    let chunks = [RetainedEmbeddingChunk {
        chunk_id: "public-link".to_owned(),
        text: format!("![public](kio://scope-a/object/image/{hash})").into(),
        text_hash: "public".to_owned(),
        raw_path: "public.md".to_owned(),
        requires_secret_approval: false,
        is_head_owner: false,
    }];
    let authority = ImageAuthorityMap::test_map(BTreeMap::from([(
        hash.clone(),
        ImageAuthorityEntry {
            policy_allowed: true,
            requires_secret_approval: true,
        },
    )]));

    assert_eq!(
        referenced_authorized_image_hashes_for_chunks(&chunks, "scope-a", &authority, false),
        BTreeSet::new(),
        "a public citation cannot bypass the secret owner's send approval"
    );
    assert_eq!(
        referenced_authorized_image_hashes_for_chunks(&chunks, "scope-a", &authority, true),
        BTreeSet::from([hash])
    );
}
