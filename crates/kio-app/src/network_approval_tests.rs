//! Device-grant contracts for external adapter permissions.
use kio_adapter::tool_lock::AdapterRuntimeSettings;
use kio_core::control::ControlHandle;

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, MutexGuard};

use kio_core::scope::Repository;
use kio_core::store_dir::StoreDirectory;
use kio_pipeline::scan::BoundPlannedChild;
use serde_json::Value;

use crate::commands::{AdapterStatusArgs, AdapterTarget, ApproveArgs, RevokeArgs};
use crate::context::{AppContext, Interaction};

const TOOL_ID: &str = "mistral_ocr_markdownize";

struct EnvironmentGuard {
    variables: Vec<(&'static str, Option<OsString>)>,
}

impl EnvironmentGuard {
    fn enter(data_home: &Path) -> Self {
        let variables = [
            "HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_CACHE_HOME",
            "KIO_TEST_LOCAL_OCR",
            "KIO_TEST_MISTRAL_OCR",
            "KIO_TEST_GEMINI_EMBED",
            "KIO_EVAL_DETERMINISTIC_EMBED",
        ]
        .into_iter()
        .map(|name| (name, std::env::var_os(name)))
        .collect();
        // SAFETY: the test-wide lock covers every process-global environment
        // mutation in this module; Drop restores all prior values before
        // releasing that lock.
        unsafe {
            std::env::set_var("HOME", data_home);
            std::env::set_var("XDG_CONFIG_HOME", data_home.join("config"));
            std::env::set_var("XDG_DATA_HOME", data_home);
            std::env::set_var("XDG_CACHE_HOME", data_home.join("cache"));
            std::env::remove_var("KIO_TEST_LOCAL_OCR");
            std::env::remove_var("KIO_TEST_MISTRAL_OCR");
            std::env::remove_var("KIO_TEST_GEMINI_EMBED");
            std::env::remove_var("KIO_EVAL_DETERMINISTIC_EMBED");
        }
        Self { variables }
    }
}

impl Drop for EnvironmentGuard {
    fn drop(&mut self) {
        // SAFETY: paired with `enter`; the module-wide lock remains held for
        // this fixture's whole lifetime.
        unsafe {
            for (name, value) in &self.variables {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
}

struct Fixture {
    _environment: EnvironmentGuard,
    _temp: tempfile::TempDir,
    repo: Repository,
    root: PathBuf,
    _lock: MutexGuard<'static, ()>,
}

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

impl Fixture {
    fn new() -> Self {
        let lock = kio_core::test_control::test_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let temp = tempfile::tempdir().unwrap();
        // Use one stable spelling for scope identity and the test's HOME/XDG.
        // The external temporary-directory ancestors are outside Kio ownership.
        let base = temp.path().canonicalize().unwrap();
        let root = base.join("scope");
        fs::create_dir(&root).unwrap();
        let control = ControlHandle::new(kio_core::test_control::DebugTestControl::default());
        let repo = match crate::management::initialize_explicit_root(&root, &control).unwrap() {
            crate::management::ExplicitRoot::Created(repo) => repo,
            crate::management::ExplicitRoot::Existing(_) => panic!("fresh scope already exists"),
        };
        let environment = EnvironmentGuard::enter(&base.join("data"));
        Self {
            _environment: environment,
            _temp: temp,
            repo,
            root,
            _lock: lock,
        }
    }

    fn grant_path(&self) -> PathBuf {
        super::approvals::store_path(self.repo.control())
    }

    fn metadata_snapshot(&self) -> Vec<(PathBuf, Vec<u8>)> {
        snapshot_tree(self.repo.kio_dir())
    }

    fn approve(&self, adapter_settings: &Arc<AdapterRuntimeSettings>, send_secrets: bool) -> Value {
        self.approve_with(adapter_settings, AdapterTarget::All, None, send_secrets)
            .unwrap()
    }

    fn approve_with(
        &self,
        adapter_settings: &Arc<AdapterRuntimeSettings>,
        target: AdapterTarget,
        resume: Option<String>,
        send_secrets: bool,
    ) -> kio_core::Result<Value> {
        self.approve_with_control(adapter_settings, target, resume, send_secrets, None)
    }

    fn approve_with_control(
        &self,
        adapter_settings: &Arc<AdapterRuntimeSettings>,
        target: AdapterTarget,
        resume: Option<String>,
        send_secrets: bool,
        stop_after: Option<&'static str>,
    ) -> kio_core::Result<Value> {
        self.in_context(adapter_settings, |ctx| {
            super::approvals::approve(
                ctx,
                ApproveArgs {
                    target,
                    preview: false,
                    yes: true,
                    resume,
                    send_secrets,
                },
                &super::approvals::ApprovalControl { stop_after },
            )
            .map(|response| {
                serde_json::to_value(response.body).expect("cfg(test) approval body projection")
            })
        })
    }

    fn status(&self, adapter_settings: &Arc<AdapterRuntimeSettings>) -> Value {
        self.in_context(adapter_settings, |ctx| {
            super::approvals::status(ctx, AdapterStatusArgs { tool_id: None }).map(|response| {
                serde_json::to_value(response.body).expect("cfg(test) approval body projection")
            })
        })
        .unwrap()
    }

    fn revoke(&self) -> Value {
        let adapter_settings = &Arc::new(AdapterRuntimeSettings::default());
        self.in_context(adapter_settings, |ctx| {
            super::approvals::revoke(
                ctx,
                RevokeArgs {
                    target: AdapterTarget::Tool(TOOL_ID.to_owned()),
                },
                ctx.control,
            )
            .map(|response| {
                serde_json::to_value(response.body).expect("cfg(test) approval body projection")
            })
        })
        .unwrap()
    }

    fn active_profile(&self) -> String {
        self.repo
            .read_network_approvals()
            .unwrap()
            .into_iter()
            .find(|row| row["tool_id"].as_str() == Some(TOOL_ID))
            .unwrap()["tool_profile_hash"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn network_allowed(
        &self,
        adapter_settings: &Arc<AdapterRuntimeSettings>,
        profile: &str,
    ) -> bool {
        crate::egress::persistent_network_allowed_for(
            adapter_settings,
            &self.repo,
            super::approvals::AdapterRole::Markdown,
            TOOL_ID,
            profile,
        )
        .unwrap()
    }

    fn in_context<T>(
        &self,
        settings: &Arc<AdapterRuntimeSettings>,
        operation: impl FnOnce(&crate::invocation::Invocation<'_>) -> kio_core::Result<T>,
    ) -> kio_core::Result<T> {
        self.in_context_at(settings, &self.root, operation)
    }

    fn in_context_at<T>(
        &self,
        settings: &Arc<AdapterRuntimeSettings>,
        root: &Path,
        operation: impl FnOnce(&crate::invocation::Invocation<'_>) -> kio_core::Result<T>,
    ) -> kio_core::Result<T> {
        let context = AppContext {
            working_directory: root.to_path_buf(),
            interaction: Arc::new(TestInteraction),
        };
        operation(&crate::invocation::Invocation {
            context: &context,
            settings,
            control: self.repo.control(),
            locks: &kio_core::scope::LockSession::default(),
        })
    }
}

fn bound_child(path: &Path) -> BoundPlannedChild {
    let canonical_root = path.canonicalize().unwrap();
    let root = StoreDirectory::open(&canonical_root)
        .unwrap()
        .root_handle()
        .as_ref()
        .try_clone()
        .unwrap();
    BoundPlannedChild {
        canonical_root,
        root,
        inherited_rules: Vec::new(),
    }
}

fn registration_generation(repo: &Repository) -> u64 {
    let binding = crate::management::binding_for_repo(repo).unwrap();
    kio_core::management::read_record(&binding)
        .unwrap()
        .registration_generation
}

fn advance_registration_generation(repo: &Repository) {
    let binding = crate::management::binding_for_repo(repo).unwrap();
    let before = kio_core::management::read_record(&binding).unwrap();
    let mut after = before.clone();
    after.registration_generation += 1;
    let operation_id = "test-registration-generation-transition";
    kio_core::management::begin_registration(&binding, operation_id).unwrap();
    kio_core::management::publish_registration_record(&binding, operation_id, &before, &after)
        .unwrap();
    kio_core::management::finish_registration(&binding, operation_id, &after).unwrap();
}

fn network_grant_id(status: &Value) -> String {
    network_grant(status)["grant_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn network_grant(status: &Value) -> &Value {
    status["grants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|grant| {
            grant["binding"]["tool_id"].as_str() == Some(TOOL_ID)
                && grant["binding"]["operation"].as_str() == Some("network")
        })
        .unwrap_or_else(|| panic!("network grant missing from status: {status}"))
}

fn network_grant_profile(status: &Value) -> String {
    network_grant(status)["binding"]["tool_profile_hash"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn snapshot_tree(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn visit(root: &Path, current: &Path, output: &mut Vec<(PathBuf, Vec<u8>)>) {
        let mut entries = fs::read_dir(current)
            .unwrap()
            .map(|entry| entry.unwrap())
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                visit(root, &path, output);
            } else {
                output.push((
                    path.strip_prefix(root).unwrap().to_owned(),
                    fs::read(path).unwrap(),
                ));
            }
        }
    }

    let mut output = Vec::new();
    visit(root, root, &mut output);
    output
}

#[test]
fn revoke_rejects_an_invalid_target_before_repository_access() {
    let error = super::approvals::revoke(
        &crate::context::test_support::context(Path::new("unused-argument-validation")),
        RevokeArgs {
            target: AdapterTarget::Tool("\n".to_owned()),
        },
        &ControlHandle::default(),
    )
    .map(|response| {
        serde_json::to_value(response.body).expect("cfg(test) approval body projection")
    })
    .expect_err("control characters are not adapter identifiers");
    assert_eq!(error.error_code(), "KIO-E-CONFIG-USAGE-001");
}

#[test]
fn preview_and_status_are_readonly_and_do_not_create_or_repair_grants() {
    let adapter_settings =
        &std::sync::Arc::new(kio_adapter::tool_lock::AdapterRuntimeSettings::default());

    let fixture = Fixture::new();
    let before = fixture.metadata_snapshot();
    assert!(!fixture.grant_path().exists());

    let preview = fixture
        .in_context(adapter_settings, |ctx| {
            super::approvals::approve(
                ctx,
                ApproveArgs {
                    target: AdapterTarget::All,
                    preview: true,
                    yes: false,
                    resume: None,
                    send_secrets: true,
                },
                &super::approvals::ApprovalControl::default(),
            )
            .map(|response| {
                serde_json::to_value(response.body).expect("cfg(test) approval body projection")
            })
        })
        .unwrap();
    assert_eq!(preview["status"], "preview");
    let status = fixture.status(adapter_settings);
    assert_eq!(
        status["scope_id"],
        fixture.repo.scope_identity().unwrap().scope_id
    );
    assert_eq!(status["grants"], Value::Array(Vec::new()));

    assert_eq!(fixture.metadata_snapshot(), before);
    assert!(!fixture.grant_path().exists());
}

#[test]
fn interrupted_approval_recovers_only_with_the_exact_pending_pair_and_resume_selection() {
    let adapter_settings =
        &std::sync::Arc::new(kio_adapter::tool_lock::AdapterRuntimeSettings::default());

    for phase in [
        "device_pending",
        "scope_pending",
        "scope_config",
        "scope_published",
        "device_active",
    ] {
        let fixture = Fixture::new();
        let error = fixture
            .approve_with_control(
                adapter_settings,
                AdapterTarget::Tool(TOOL_ID.to_owned()),
                None,
                true,
                Some(phase),
            )
            .unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-APPROVAL-TEST-INTERRUPTED-001");
        assert_eq!(error.context()["phase"], phase);

        let status = fixture.status(adapter_settings);
        let network_id = network_grant_id(&status);
        let profile = network_grant_profile(&status);
        let scope_rows = fixture.repo.read_network_approvals().unwrap();
        let scope_pending = fixture.repo.read_network_approval_pending().unwrap();
        let network_policy_open = fixture
            .repo
            .read_config_document()
            .unwrap()
            .contains("allow_network = true");
        match phase {
            "device_pending" => {
                assert!(scope_rows.is_empty());
                assert_eq!(scope_pending, None);
                assert!(!network_policy_open);
            }
            "scope_pending" | "scope_config" => {
                assert!(scope_rows.is_empty());
                assert_eq!(
                    scope_pending
                        .as_ref()
                        .and_then(|pending| pending["grant_id"].as_str()),
                    Some(network_id.as_str())
                );
                assert_eq!(network_policy_open, phase == "scope_config");
            }
            "scope_published" | "device_active" => {
                assert_eq!(scope_rows.len(), 1);
                assert_eq!(scope_rows[0]["status"], "active");
                assert_eq!(scope_pending, None);
                assert!(network_policy_open);
            }
            _ => unreachable!("fixed STOP_AFTER phase"),
        }
        if phase != "device_active" {
            assert!(
                !fixture.network_allowed(adapter_settings, &profile),
                "{phase} must not authorize before device activation"
            );
            assert!(
                !super::approvals::secrets_allowed(
                    adapter_settings,
                    &fixture.repo,
                    super::approvals::AdapterRole::Markdown,
                    TOOL_ID,
                    &profile
                )
                .unwrap()
            );
        }

        let resumed = fixture
            .approve_with(
                adapter_settings,
                AdapterTarget::Tool(TOOL_ID.to_owned()),
                Some(network_id),
                true,
            )
            .unwrap();
        assert_eq!(resumed["status"], "approved");
        assert!(fixture.network_allowed(adapter_settings, &fixture.active_profile()));
        assert!(
            super::approvals::secrets_allowed(
                adapter_settings,
                &fixture.repo,
                super::approvals::AdapterRole::Markdown,
                TOOL_ID,
                &fixture.active_profile(),
            )
            .unwrap()
        );
    }
}

#[test]
fn revoke_after_an_interrupted_pair_prevents_resume_and_any_send() {
    let adapter_settings =
        &std::sync::Arc::new(kio_adapter::tool_lock::AdapterRuntimeSettings::default());

    let fixture = Fixture::new();
    let error = fixture
        .approve_with_control(
            adapter_settings,
            AdapterTarget::Tool(TOOL_ID.to_owned()),
            None,
            true,
            Some("scope_pending"),
        )
        .unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-APPROVAL-TEST-INTERRUPTED-001");

    let status = fixture.status(adapter_settings);
    let network_id = network_grant_id(&status);
    let profile = network_grant_profile(&status);
    let revoked = fixture.revoke();
    assert_eq!(revoked["status"], "revoked");
    let error = fixture
        .approve_with(
            adapter_settings,
            AdapterTarget::Tool(TOOL_ID.to_owned()),
            Some(network_id),
            true,
        )
        .unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-ADAPTER-APPROVAL-REQUIRED-001");
    assert!(!fixture.network_allowed(adapter_settings, &profile));
    assert!(
        !super::approvals::secrets_allowed(
            adapter_settings,
            &fixture.repo,
            super::approvals::AdapterRole::Markdown,
            TOOL_ID,
            &profile
        )
        .unwrap()
    );
}

#[test]
fn copied_scope_approval_reference_cannot_reuse_the_source_device_grant() {
    let adapter_settings =
        &std::sync::Arc::new(kio_adapter::tool_lock::AdapterRuntimeSettings::default());

    let fixture = Fixture::new();
    fixture.approve(adapter_settings, false);
    let profile = fixture.active_profile();
    assert!(fixture.network_allowed(adapter_settings, &profile));

    let copied_root = fixture.root.parent().unwrap().join("copied-scope");
    fs::create_dir(&copied_root).unwrap();
    let copied =
        match crate::management::initialize_explicit_root(&copied_root, &ControlHandle::default())
            .unwrap()
        {
            crate::management::ExplicitRoot::Created(repo) => repo,
            crate::management::ExplicitRoot::Existing(_) => panic!("copied fixture already exists"),
        };
    // Model a portable copied scope approval without copying the device-private
    // grant. Its source scope identity remains in the portable reference.
    let source_row = fixture
        .repo
        .read_network_approvals()
        .unwrap()
        .pop()
        .unwrap();
    copied.publish_network_approval(source_row, None).unwrap();

    assert_eq!(copied.read_network_approvals().unwrap().len(), 1);
    assert!(
        !crate::egress::persistent_network_allowed_for(
            adapter_settings,
            &copied,
            super::approvals::AdapterRole::Markdown,
            TOOL_ID,
            &profile
        )
        .unwrap()
    );
}

#[test]
fn profile_or_network_policy_change_denies_until_an_explicit_approval_command() {
    let adapter_settings =
        &std::sync::Arc::new(kio_adapter::tool_lock::AdapterRuntimeSettings::default());

    let fixture = Fixture::new();
    fixture.approve(adapter_settings, false);
    let profile = fixture.active_profile();
    assert!(fixture.network_allowed(adapter_settings, &profile));
    assert!(!fixture.network_allowed(adapter_settings, "sha256:drifted-profile"));

    let old = fixture.repo.read_config_document().unwrap();
    fixture
        .repo
        .compare_replace_config_document(&old, "[adapter.policy]\nallow_network = false\n")
        .unwrap();
    assert!(!fixture.network_allowed(adapter_settings, &profile));

    let outcome = fixture.approve(adapter_settings, false);
    assert_eq!(outcome["status"], "approved");
    assert!(fixture.network_allowed(adapter_settings, &profile));
}

#[test]
fn embedding_grants_require_the_explicit_current_role_and_profile() {
    use crate::approvals::AdapterRole;
    use crate::approvals::allowed;
    use crate::grants::GrantOperation;
    use kio_adapter::tool_lock::{AdapterRuntimeSettings, DeclaredAdapter};
    use kio_core::test_control::{DebugTestControl, GeminiEmbedMode, Selector};

    let fixture = Fixture::new();
    let control = fixture.repo.control();
    let settings = AdapterRuntimeSettings {
        declarations: [(
            "embedding".to_owned(),
            DeclaredAdapter {
                tool_id: Some("gemini_embedding_2".to_owned()),
                auth: Some("env:KIO_TEST_APPROVAL_EMBED_KEY".to_owned()),
                ..Default::default()
            },
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    {
        let adapter_settings = &std::sync::Arc::new(settings);
        let profile = crate::embedding::admission::declared_embedding_profile(
            crate::embedding::admission::embedding_execution(adapter_settings, control).unwrap(),
        );
        let target = AdapterTarget::Tool(profile.tool_id.clone());
        let preview = fixture
            .in_context(adapter_settings, |ctx| {
                super::approvals::approve(
                    ctx,
                    ApproveArgs {
                        target: target.clone(),
                        preview: true,
                        yes: false,
                        resume: None,
                        send_secrets: true,
                    },
                    &super::approvals::ApprovalControl::default(),
                )
                .map(|response| {
                    serde_json::to_value(response.body).expect("cfg(test) approval body projection")
                })
            })
            .unwrap();
        assert_eq!(preview["adapters"][0]["tool_id"], profile.tool_id);
        assert!(
            preview["adapters"][0].get("role").is_none(),
            "role does not alter preview schema"
        );
        fixture
            .approve_with(adapter_settings, target, None, true)
            .unwrap();
        let grants = fs::read(fixture.grant_path()).unwrap();
        let scope_approvals = fixture.repo.read_network_approvals().unwrap();
        let query = |adapter_settings: &Arc<AdapterRuntimeSettings>,
                     role,
                     requested_profile: &str,
                     operation| {
            allowed(
                adapter_settings,
                &fixture.repo,
                role,
                &profile.tool_id,
                requested_profile,
                operation,
            )
        };
        for operation in [GrantOperation::Network, GrantOperation::SendSecrets] {
            assert!(
                query(
                    adapter_settings,
                    AdapterRole::Embedding,
                    &profile.profile_hash,
                    operation
                )
                .unwrap()
            );
            assert!(
                !query(
                    adapter_settings,
                    AdapterRole::Markdown,
                    &profile.profile_hash,
                    operation
                )
                .unwrap()
            );
            assert!(
                !query(
                    adapter_settings,
                    AdapterRole::Embedding,
                    "sha256:drifted-profile",
                    operation
                )
                .unwrap()
            );
        }
        assert!(
            fixture.status(adapter_settings)["effective"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["permitted"] == true)
        );

        // Persisted embedding grants cannot borrow Markdown runtime authority
        // when rebuild has no active embedding adapter.
        {
            let adapter_settings = &std::sync::Arc::new(AdapterRuntimeSettings::default());
            assert!(
                crate::embedding::admission::embedding_execution(adapter_settings, control)
                    .is_none()
            );
            for operation in [GrantOperation::Network, GrantOperation::SendSecrets] {
                assert!(
                    !query(
                        adapter_settings,
                        AdapterRole::Embedding,
                        &profile.profile_hash,
                        operation
                    )
                    .unwrap()
                );
            }
            assert!(
                fixture.status(adapter_settings)["effective"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|row| row["permitted"] == false)
            );
        };
        {
            // The existing CLI rebuild regression uses this mismatched profile.
            let mut control = DebugTestControl::default();
            control.adapters.gemini_embed = Selector::Known(GeminiEmbedMode::IncompatibleProfile);
            let drift = ControlHandle::new(control);
            let drift_repo = fixture.repo.rebind_with_control(drift.clone());
            let current = crate::embedding::admission::declared_embedding_profile(
                crate::embedding::admission::embedding_execution(adapter_settings, &drift).unwrap(),
            );
            assert_ne!(current.profile_hash, profile.profile_hash);
            for operation in [GrantOperation::Network, GrantOperation::SendSecrets] {
                assert!(
                    !allowed(
                        adapter_settings,
                        &drift_repo,
                        AdapterRole::Embedding,
                        &profile.tool_id,
                        &profile.profile_hash,
                        operation
                    )
                    .unwrap()
                );
            }
        }
        // Querying absence/drift never rewrites grants; restoring the exact
        // currently configured identity restores its existing exact approval.
        assert!(
            query(
                adapter_settings,
                AdapterRole::Embedding,
                &profile.profile_hash,
                GrantOperation::SendSecrets
            )
            .unwrap()
        );
        assert_eq!(fs::read(fixture.grant_path()).unwrap(), grants);
        assert_eq!(
            fixture.repo.read_network_approvals().unwrap(),
            scope_approvals
        );
    };
}

#[test]
fn approval_query_preserves_current_runtime_and_config_errors() {
    let adapter_settings = &Arc::new(AdapterRuntimeSettings::default());

    use crate::approvals::AdapterRole;
    use crate::approvals::allowed;
    use crate::grants::GrantOperation;
    use kio_adapter::tool_lock::{AdapterRuntimeSettings, DeclaredAdapter};
    let fixture = Fixture::new();
    fixture.approve(adapter_settings, true);
    let profile = fixture.active_profile();
    let malformed = AdapterRuntimeSettings {
        declarations: [(
            "markdown".to_owned(),
            DeclaredAdapter {
                tool_id: Some(TOOL_ID.to_owned()),
                kind: Some("online_api".to_owned()),
                url: Some("https://unexpected.example".to_owned()),
                ..Default::default()
            },
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    {
        let adapter_settings = &std::sync::Arc::new(malformed);
        assert!(
            allowed(
                adapter_settings,
                &fixture.repo,
                AdapterRole::Markdown,
                TOOL_ID,
                &profile,
                GrantOperation::SendSecrets
            )
            .is_err()
        );
    };
    let before = fixture.repo.read_config_document().unwrap();
    // Simulate external corruption; the production replacement API correctly
    // refuses malformed config before it can be written.
    fs::write(fixture.repo.kio_dir().join("config.toml"), "[invalid").unwrap();
    assert!(
        allowed(
            adapter_settings,
            &fixture.repo,
            AdapterRole::Markdown,
            TOOL_ID,
            &profile,
            GrantOperation::SendSecrets
        )
        .is_err()
    );
    fs::write(fixture.repo.kio_dir().join("config.toml"), &before).unwrap();
    assert!(
        allowed(
            adapter_settings,
            &fixture.repo,
            AdapterRole::Markdown,
            TOOL_ID,
            &profile,
            GrantOperation::SendSecrets
        )
        .unwrap()
    );
}

#[test]
fn network_and_send_secrets_grants_are_distinct_permissions() {
    let adapter_settings =
        &std::sync::Arc::new(kio_adapter::tool_lock::AdapterRuntimeSettings::default());

    let fixture = Fixture::new();
    fixture.approve(adapter_settings, false);
    let profile = fixture.active_profile();
    assert!(fixture.network_allowed(adapter_settings, &profile));
    assert!(
        !super::approvals::secrets_allowed(
            adapter_settings,
            &fixture.repo,
            super::approvals::AdapterRole::Markdown,
            TOOL_ID,
            &profile
        )
        .unwrap()
    );

    fixture.approve(adapter_settings, true);
    assert!(fixture.network_allowed(adapter_settings, &fixture.active_profile()));
    assert!(
        super::approvals::secrets_allowed(
            adapter_settings,
            &fixture.repo,
            super::approvals::AdapterRole::Markdown,
            TOOL_ID,
            &fixture.active_profile()
        )
        .unwrap()
    );
}

#[test]
fn reregistration_generation_invalidates_network_and_secret_grants_without_path_change() {
    let adapter_settings =
        &std::sync::Arc::new(kio_adapter::tool_lock::AdapterRuntimeSettings::default());

    let fixture = Fixture::new();
    fixture.approve(adapter_settings, true);
    let profile = fixture.active_profile();
    let before = crate::management::binding_for_repo(&fixture.repo).unwrap();
    let generation = registration_generation(&fixture.repo);
    let grant_bytes = fs::read(fixture.grant_path()).unwrap();
    let scope_rows = fixture.repo.read_network_approvals().unwrap();
    assert!(fixture.network_allowed(adapter_settings, &profile));
    assert!(
        super::approvals::secrets_allowed(
            adapter_settings,
            &fixture.repo,
            super::approvals::AdapterRole::Markdown,
            TOOL_ID,
            &profile
        )
        .unwrap()
    );

    advance_registration_generation(&fixture.repo);

    let after = crate::management::binding_for_repo(&fixture.repo).unwrap();
    assert_eq!(before.canonical_root(), after.canonical_root());
    assert_eq!(before.directory_identity(), after.directory_identity());
    assert_eq!(registration_generation(&fixture.repo), generation + 1);
    assert_eq!(fs::read(fixture.grant_path()).unwrap(), grant_bytes);
    assert_eq!(fixture.repo.read_network_approvals().unwrap(), scope_rows);
    assert!(!fixture.network_allowed(adapter_settings, &profile));
    assert!(
        !super::approvals::secrets_allowed(
            adapter_settings,
            &fixture.repo,
            super::approvals::AdapterRole::Markdown,
            TOOL_ID,
            &profile
        )
        .unwrap()
    );
}

#[test]
fn ancestor_reregistration_invalidates_a_childs_network_and_secret_grants() {
    let adapter_settings =
        &std::sync::Arc::new(kio_adapter::tool_lock::AdapterRuntimeSettings::default());

    let fixture = Fixture::new();
    let child_path = fixture.root.join("child");
    fs::create_dir(&child_path).unwrap();
    let crate::management::ChildScope::Managed { repo: child } =
        crate::management::reconcile_planned_child(&fixture.repo, bound_child(&child_path))
            .unwrap()
    else {
        panic!("child must be managed");
    };
    fixture
        .in_context_at(adapter_settings, &child_path, |ctx| {
            super::approvals::approve(
                ctx,
                ApproveArgs {
                    target: AdapterTarget::Tool(TOOL_ID.to_owned()),
                    preview: false,
                    yes: true,
                    resume: None,
                    send_secrets: true,
                },
                &super::approvals::ApprovalControl::default(),
            )
            .map(|response| {
                serde_json::to_value(response.body).expect("cfg(test) approval body projection")
            })
        })
        .unwrap();
    let profile = child
        .read_network_approvals()
        .unwrap()
        .into_iter()
        .find(|row| row["tool_id"].as_str() == Some(TOOL_ID))
        .unwrap()["tool_profile_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    let before = crate::management::binding_for_repo(&child).unwrap();
    let generation = registration_generation(&child);
    let parent_generation = registration_generation(&fixture.repo);
    let grant_bytes = fs::read(fixture.grant_path()).unwrap();
    let scope_rows = child.read_network_approvals().unwrap();
    assert!(
        crate::egress::persistent_network_allowed_for(
            adapter_settings,
            &child,
            super::approvals::AdapterRole::Markdown,
            TOOL_ID,
            &profile
        )
        .unwrap()
    );
    assert!(
        super::approvals::secrets_allowed(
            adapter_settings,
            &child,
            super::approvals::AdapterRole::Markdown,
            TOOL_ID,
            &profile
        )
        .unwrap()
    );

    advance_registration_generation(&fixture.repo);

    let after = crate::management::binding_for_repo(&child).unwrap();
    assert_eq!(before.canonical_root(), after.canonical_root());
    assert_eq!(before.directory_identity(), after.directory_identity());
    assert_eq!(registration_generation(&child), generation);
    assert_eq!(
        registration_generation(&fixture.repo),
        parent_generation + 1
    );
    assert_eq!(fs::read(fixture.grant_path()).unwrap(), grant_bytes);
    assert_eq!(child.read_network_approvals().unwrap(), scope_rows);
    assert!(
        !crate::egress::persistent_network_allowed_for(
            adapter_settings,
            &child,
            super::approvals::AdapterRole::Markdown,
            TOOL_ID,
            &profile
        )
        .unwrap()
    );
    assert!(
        !super::approvals::secrets_allowed(
            adapter_settings,
            &child,
            super::approvals::AdapterRole::Markdown,
            TOOL_ID,
            &profile
        )
        .unwrap()
    );
}

#[test]
fn reregistration_makes_an_old_pending_approval_unresumable() {
    let adapter_settings =
        &std::sync::Arc::new(kio_adapter::tool_lock::AdapterRuntimeSettings::default());

    let fixture = Fixture::new();
    let error = fixture
        .approve_with_control(
            adapter_settings,
            AdapterTarget::Tool(TOOL_ID.to_owned()),
            None,
            true,
            Some("scope_pending"),
        )
        .unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-APPROVAL-TEST-INTERRUPTED-001");
    let pending = network_grant_id(&fixture.status(adapter_settings));
    let grant_bytes = fs::read(fixture.grant_path()).unwrap();
    let pending_scope = fixture.repo.read_network_approval_pending().unwrap();

    advance_registration_generation(&fixture.repo);
    assert_eq!(fs::read(fixture.grant_path()).unwrap(), grant_bytes);
    assert_eq!(
        fixture.repo.read_network_approval_pending().unwrap(),
        pending_scope
    );

    let error = fixture
        .approve_with(
            adapter_settings,
            AdapterTarget::Tool(TOOL_ID.to_owned()),
            Some(pending),
            true,
        )
        .unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-ADAPTER-APPROVAL-REQUIRED-001");
}

#[derive(Clone, Copy)]
enum NestedApprovalOutcome {
    Success,
    Error,
    Panic,
}

type ConfirmationCallback = dyn Fn(&str) -> kio_core::Result<bool> + Send + Sync;

struct CallbackInteraction {
    confirm: Box<ConfirmationCallback>,
}

impl Interaction for CallbackInteraction {
    fn confirm(&self, summary: &str) -> kio_core::Result<bool> {
        (self.confirm)(summary)
    }

    fn read_input(&self, limit: usize) -> kio_core::Result<Vec<u8>> {
        TestInteraction.read_input(limit)
    }

    fn is_interactive(&self) -> bool {
        true
    }

    fn diagnostic(&self, message: &str) {
        TestInteraction.diagnostic(message);
    }

    fn write_diagnostic(&self, text: &str) -> kio_core::Result<()> {
        TestInteraction.write_diagnostic(text)
    }

    fn flush_diagnostics(&self) -> kio_core::Result<()> {
        TestInteraction.flush_diagnostics()
    }
}

fn nested_approval_command(target: AdapterTarget) -> crate::commands::CommandRequest {
    crate::commands::CommandRequest::Adapter(crate::commands::AdapterArgs {
        command: Some(crate::commands::AdapterCommand::Approve(ApproveArgs {
            target,
            preview: false,
            yes: false,
            resume: None,
            send_secrets: false,
        })),
    })
}

fn assert_nested_approval_keeps_outer_control(outcome: NestedApprovalOutcome) {
    use std::sync::atomic::{AtomicBool, Ordering};

    const OUTER_NOW: &str = "2026-08-25T00:00:00Z";
    const INNER_NOW: &str = "2026-08-26T00:00:00Z";
    let fixture = Fixture::new();
    let _fixed_now = kio_core::test_control::TestEnvGuard::set("KIO_FIXED_NOW", OUTER_NOW);
    let _mistral = kio_core::test_control::TestEnvGuard::set("KIO_TEST_MISTRAL_OCR", "mock");
    let _gemini = kio_core::test_control::TestEnvGuard::remove("KIO_TEST_GEMINI_EMBED");
    let inner_root = fixture.root.with_file_name("inner-scope");
    fs::create_dir(&inner_root).unwrap();
    let inner_repo =
        match crate::management::initialize_explicit_root(&inner_root, &ControlHandle::default())
            .unwrap()
        {
            crate::management::ExplicitRoot::Created(repo) => repo,
            crate::management::ExplicitRoot::Existing(_) => {
                panic!("fresh inner scope already exists")
            }
        };
    let inner_observed_fresh_selector = Arc::new(AtomicBool::new(false));
    let observed = inner_observed_fresh_selector.clone();
    let embedding_tool = kio_adapter::catalog::adopted_embedding_profile().adapter_id;
    let outer_context = AppContext {
        working_directory: fixture.root.clone(),
        interaction: Arc::new(CallbackInteraction {
            confirm: Box::new(move |outer_preview| {
                assert!(!outer_preview.contains(&embedding_tool));
                // The fixture's test_env_lock covers both executes. Leave B
                // installed until the outer publication, so recapture cannot
                // accidentally satisfy the outer A assertion.
                unsafe {
                    std::env::set_var("KIO_FIXED_NOW", INNER_NOW);
                    std::env::set_var("KIO_TEST_GEMINI_EMBED", "mock");
                }
                let observed = observed.clone();
                let embedding_tool = embedding_tool.clone();
                let inner_context = AppContext {
                    working_directory: inner_root.clone(),
                    interaction: Arc::new(CallbackInteraction {
                        confirm: Box::new(move |inner_preview| {
                            assert!(
                                inner_preview.contains(&embedding_tool),
                                "inner execute must capture its fresh Gemini debug selector"
                            );
                            observed.store(true, Ordering::SeqCst);
                            match outcome {
                                NestedApprovalOutcome::Success => Ok(true),
                                NestedApprovalOutcome::Error => Err(
                                    kio_core::KioError::invalid_usage("nested confirmation error"),
                                ),
                                NestedApprovalOutcome::Panic => panic!("nested confirmation panic"),
                            }
                        }),
                    }),
                };
                let inner_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    crate::execute(&inner_context, nested_approval_command(AdapterTarget::All))
                }));
                match outcome {
                    NestedApprovalOutcome::Success => {
                        inner_result.unwrap().unwrap();
                    }
                    NestedApprovalOutcome::Error => {
                        assert!(
                            inner_result
                                .unwrap()
                                .unwrap_err()
                                .to_string()
                                .contains("nested confirmation error")
                        );
                    }
                    NestedApprovalOutcome::Panic => assert!(inner_result.is_err()),
                }
                Ok(true)
            }),
        }),
    };
    crate::execute(
        &outer_context,
        nested_approval_command(AdapterTarget::Tool(TOOL_ID.to_owned())),
    )
    .unwrap();
    assert!(inner_observed_fresh_selector.load(Ordering::SeqCst));
    assert_eq!(std::env::var("KIO_FIXED_NOW").unwrap(), INNER_NOW);
    let inner_approvals = inner_repo.read_network_approvals().unwrap();
    match outcome {
        NestedApprovalOutcome::Success => {
            assert_eq!(inner_approvals.len(), 2);
            for approval in inner_approvals {
                assert_eq!(approval["approved_at"], INNER_NOW);
            }
        }
        NestedApprovalOutcome::Error | NestedApprovalOutcome::Panic => {
            assert!(inner_approvals.is_empty());
        }
    }
    let outer_approvals = fixture.repo.read_network_approvals().unwrap();
    assert_eq!(outer_approvals.len(), 1);
    assert_eq!(
        outer_approvals[0]["approved_at"], OUTER_NOW,
        "outer execute must retain its original clock after the inner execute"
    );
}

#[test]
fn nested_execute_preserves_outer_control_after_inner_success() {
    assert_nested_approval_keeps_outer_control(NestedApprovalOutcome::Success);
}

#[test]
fn nested_execute_preserves_outer_control_after_inner_error() {
    assert_nested_approval_keeps_outer_control(NestedApprovalOutcome::Error);
}

#[test]
fn nested_execute_preserves_outer_control_after_inner_panic() {
    assert_nested_approval_keeps_outer_control(NestedApprovalOutcome::Panic);
}

const LOOPBACK_TEST_CA: &str = "-----BEGIN CERTIFICATE-----\nMIIBWTCB/6ADAgECAhR5P0J0YMFaZPlrOFyN8/jwpGzhqjAKBggqhkjOPQQDAjAh\nMR8wHQYDVQQDDBZyY2dlbiBzZWxmIHNpZ25lZCBjZXJ0MCAXDTc1MDEwMTAwMDAw\nMFoYDzQwOTYwMTAxMDAwMDAwWjAhMR8wHQYDVQQDDBZyY2dlbiBzZWxmIHNpZ25l\nZCBjZXJ0MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEGhIwEPQnvWSlH+iUQ5Ui\nq0khmBj4hlxmW+mTL5xjjyxwwopOnkxLs2AofasS2lYPdILzMaIeRE78g2S7Hz1n\nX6MTMBEwDwYDVR0RBAgwBocEfwAAATAKBggqhkjOPQQDAgNJADBGAiEA0GBG4uEL\nSJJea18R17+yhRTyn3rsD6PzDhdg7F/v2sQCIQCccgcgjDh+ABNajeb1deKZoqbx\nnP4qBrGe09azOI4jbg==\n-----END CERTIFICATE-----\n";

fn loopback_settings(fixture: &Fixture) -> Arc<AdapterRuntimeSettings> {
    use kio_adapter::tool_lock::DeclaredAdapter;
    use std::io::Write;
    let ca = fixture.repo.kio_dir().join("loopback-test-ca.pem");
    let mut file = crate::test_support::create_private_test_file(&ca);
    file.write_all(LOOPBACK_TEST_CA.as_bytes()).unwrap();
    file.sync_all().unwrap();
    drop(file);
    let mut settings = AdapterRuntimeSettings::default();
    settings.local_peer_ca_path = Some(ca.clone());
    settings.local_peer_trust_digest =
        Some(kio_adapter::local_peer::capture_private_local_trust(&ca).unwrap());
    settings.local_peer_trust_binding = Some(format!(
        "local-ca:v1:g1:01ARZ3NDEKTSV4RRFFQ69G5FAV:{}",
        settings.local_peer_trust_digest.as_ref().unwrap()
    ));
    settings.declarations.insert(
        "embedding".into(),
        DeclaredAdapter {
            tool_id: Some("qwen3_vl_embedding_local".into()),
            kind: Some("offline_api".into()),
            url: Some("https://127.0.0.1:9443/v1/embeddings".into()),
            ..Default::default()
        },
    );
    Arc::new(settings)
}

#[test]
fn loopback_endpoint_grant_reuses_device_authority_across_scopes_and_revokes_everywhere() {
    use crate::grants::GrantOperation;
    let fixture = Fixture::new();
    let settings = loopback_settings(&fixture);
    let profile = crate::embedding::admission::declared_embedding_profile(
        crate::embedding::admission::embedding_execution(&settings, fixture.repo.control())
            .expect("configured local embedding execution"),
    );
    let target = AdapterTarget::Tool(profile.tool_id.clone());
    let interrupted = fixture.approve_with_control(
        &settings,
        target.clone(),
        None,
        false,
        Some("device_pending"),
    );
    assert!(
        interrupted.is_err(),
        "fixture must stop with an unconfirmed device intent"
    );
    let pending = crate::grants::PrivateGrantStore::open_readonly(
        &fixture.grant_path(),
        &crate::settings::data_home(fixture.repo.control()),
        fixture.repo.control(),
    )
    .unwrap()
    .unwrap();
    let grant_id = pending
        .records()
        .iter()
        .find(|record| record.state == crate::grants::GrantState::Pending)
        .unwrap()
        .grant_id
        .clone();
    assert!(
        !super::approvals::allowed(
            &settings,
            &fixture.repo,
            super::approvals::AdapterRole::Embedding,
            &profile.tool_id,
            &profile.profile_hash,
            GrantOperation::Network
        )
        .unwrap()
    );
    fixture
        .approve_with(&settings, target, Some(grant_id), false)
        .unwrap();
    assert!(
        super::approvals::allowed(
            &settings,
            &fixture.repo,
            super::approvals::AdapterRole::Embedding,
            &profile.tool_id,
            &profile.profile_hash,
            GrantOperation::Network
        )
        .unwrap(),
        "same-scope positive control"
    );
    let sibling_root = fixture.root.with_file_name("second-endpoint-consumer");
    fs::create_dir(&sibling_root).unwrap();
    let sibling =
        match crate::management::initialize_explicit_root(&sibling_root, fixture.repo.control())
            .unwrap()
        {
            crate::management::ExplicitRoot::Created(repo) => repo,
            _ => panic!("fresh independent scope expected"),
        };
    let before = snapshot_tree(sibling.kio_dir());
    assert!(
        super::approvals::allowed(
            &settings,
            &sibling,
            super::approvals::AdapterRole::Embedding,
            &profile.tool_id,
            &profile.profile_hash,
            GrantOperation::Network
        )
        .unwrap(),
        "one device endpoint confirmation must authorize the same endpoint in another scope without a scope grant"
    );
    assert_eq!(
        snapshot_tree(sibling.kio_dir()),
        before,
        "admission must not synthesize scope consent"
    );
    assert!(
        !super::approvals::allowed(
            &settings,
            &sibling,
            super::approvals::AdapterRole::Embedding,
            &profile.tool_id,
            &profile.profile_hash,
            GrantOperation::SendSecrets
        )
        .unwrap(),
        "ordinary endpoint consent cannot release secrets"
    );
    let mut changed = (*settings).clone();
    changed.declarations.get_mut("embedding").unwrap().url =
        Some("https://127.0.0.1:9443/other-path".into());
    assert!(
        !super::approvals::allowed(
            &Arc::new(changed),
            &sibling,
            super::approvals::AdapterRole::Embedding,
            &profile.tool_id,
            &profile.profile_hash,
            GrantOperation::Network
        )
        .unwrap(),
        "path drift requires fresh endpoint consent"
    );
    fixture
        .in_context_at(&settings, &sibling_root, |ctx| {
            super::approvals::revoke(
                ctx,
                RevokeArgs {
                    target: AdapterTarget::Tool(profile.tool_id.clone()),
                },
                ctx.control,
            )
        })
        .unwrap();
    for repo in [&fixture.repo, &sibling] {
        assert!(
            !super::approvals::allowed(
                &settings,
                repo,
                super::approvals::AdapterRole::Embedding,
                &profile.tool_id,
                &profile.profile_hash,
                GrantOperation::Network
            )
            .unwrap(),
            "device endpoint revoke must affect every scope"
        );
    }
}

#[test]
fn endpoint_secret_confirmation_is_scoped_resumable_and_requires_live_parent() {
    use crate::grants::{GrantOperation, GrantState};
    let fixture = Fixture::new();
    let settings = loopback_settings(&fixture);
    let tool = "qwen3_vl_embedding_local";
    let profile = crate::embedding::admission::declared_embedding_profile(
        crate::embedding::admission::embedding_execution(&settings, fixture.repo.control())
            .unwrap(),
    );
    fixture
        .approve_with(&settings, AdapterTarget::Tool(tool.into()), None, false)
        .unwrap();
    let second = fixture.root.with_file_name("secret-endpoint-scope");
    fs::create_dir(&second).unwrap();
    let repo = match crate::management::initialize_explicit_root(&second, fixture.repo.control())
        .unwrap()
    {
        crate::management::ExplicitRoot::Created(repo) => repo,
        _ => panic!("fresh scope"),
    };
    let approve_at = |resume: Option<String>, secrets: bool, stop_after| {
        fixture.in_context_at(&settings, &second, |ctx| {
            super::approvals::approve(
                ctx,
                ApproveArgs {
                    target: AdapterTarget::Tool(tool.into()),
                    preview: false,
                    yes: true,
                    resume,
                    send_secrets: secrets,
                },
                &super::approvals::ApprovalControl { stop_after },
            )
        })
    };
    assert!(approve_at(None, true, Some("device_pending")).is_err());
    let snapshot = crate::grants::PrivateGrantStore::open_readonly(
        &fixture.grant_path(),
        &crate::settings::data_home(fixture.repo.control()),
        fixture.repo.control(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        snapshot
            .records()
            .iter()
            .filter(|r| r.binding.operation() == GrantOperation::Network)
            .count(),
        1,
        "adding scoped secrets must not duplicate device endpoint authority"
    );
    let pending = snapshot
        .records()
        .iter()
        .find(|r| r.state == GrantState::Pending && r.parent_network_grant_id.is_some())
        .unwrap();
    let id = pending.grant_id.clone();
    assert!(
        !super::approvals::allowed(
            &settings,
            &repo,
            super::approvals::AdapterRole::Embedding,
            tool,
            &profile.profile_hash,
            GrantOperation::SendSecrets
        )
        .unwrap()
    );
    assert!(
        approve_at(Some(id.clone()), false, None).is_err(),
        "resume cannot change original secret disclosure"
    );
    approve_at(Some(id), true, None).unwrap();
    assert!(
        super::approvals::allowed(
            &settings,
            &repo,
            super::approvals::AdapterRole::Embedding,
            tool,
            &profile.profile_hash,
            GrantOperation::SendSecrets
        )
        .unwrap()
    );
    assert!(
        !super::approvals::allowed(
            &settings,
            &fixture.repo,
            super::approvals::AdapterRole::Embedding,
            tool,
            &profile.profile_hash,
            GrantOperation::SendSecrets
        )
        .unwrap()
    );
    fixture
        .in_context(&settings, |ctx| {
            super::approvals::revoke(
                ctx,
                RevokeArgs {
                    target: AdapterTarget::Tool(tool.into()),
                },
                ctx.control,
            )
        })
        .unwrap();
    assert!(
        !super::approvals::allowed(
            &settings,
            &repo,
            super::approvals::AdapterRole::Embedding,
            tool,
            &profile.profile_hash,
            GrantOperation::SendSecrets
        )
        .unwrap()
    );
}

#[test]
fn combined_endpoint_secret_pending_cannot_resume_after_parent_revoke() {
    use crate::grants::{GrantOperation, GrantState};
    let fixture = Fixture::new();
    let settings = loopback_settings(&fixture);
    let tool = "qwen3_vl_embedding_local";
    assert!(
        fixture
            .approve_with_control(
                &settings,
                AdapterTarget::Tool(tool.into()),
                None,
                true,
                Some("device_pending")
            )
            .is_err()
    );
    let snapshot = crate::grants::PrivateGrantStore::open_readonly(
        &fixture.grant_path(),
        &crate::settings::data_home(fixture.repo.control()),
        fixture.repo.control(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(snapshot.records().len(), 2);
    assert!(
        snapshot
            .records()
            .iter()
            .all(|r| r.state == GrantState::Pending)
    );
    let id = snapshot
        .records()
        .iter()
        .find(|r| r.binding.operation() == GrantOperation::Network)
        .unwrap()
        .grant_id
        .clone();
    fixture
        .in_context(&settings, |ctx| {
            super::approvals::revoke(
                ctx,
                RevokeArgs {
                    target: AdapterTarget::Tool(tool.into()),
                },
                ctx.control,
            )
        })
        .unwrap();
    assert!(
        fixture
            .approve_with(&settings, AdapterTarget::Tool(tool.into()), Some(id), true)
            .is_err()
    );
    let snapshot = crate::grants::PrivateGrantStore::open_readonly(
        &fixture.grant_path(),
        &crate::settings::data_home(fixture.repo.control()),
        fixture.repo.control(),
    )
    .unwrap()
    .unwrap();
    assert!(
        snapshot
            .records()
            .iter()
            .all(|r| r.state == GrantState::Revoked)
    );
}

#[test]
fn unused_missing_https_trust_with_unrelated_device_grants_is_optional_readiness() {
    let fixture = Fixture::new();
    fixture.approve(&Arc::new(AdapterRuntimeSettings::default()), false);
    let mut settings = (*loopback_settings(&fixture)).clone();
    settings.local_peer_trust_binding = None;
    let settings = Arc::new(settings);
    let profile = crate::embedding::admission::declared_embedding_profile(
        crate::embedding::admission::embedding_execution(&settings, fixture.repo.control())
            .unwrap(),
    );
    let before = fixture.metadata_snapshot();
    let allowed = super::approvals::allowed(
        &settings,
        &fixture.repo,
        super::approvals::AdapterRole::Embedding,
        &profile.tool_id,
        &profile.profile_hash,
        crate::grants::GrantOperation::Network,
    );
    assert!(
        matches!(allowed, Ok(false)),
        "unused local trust must be unavailable readiness, not abort local work: {allowed:?}"
    );
    assert_eq!(fixture.metadata_snapshot(), before);
}

#[cfg(unix)]
fn assert_dispatch_grant_directory_binding(active_original: bool) {
    use crate::grants::{GrantBinding, PrivateGrantStore};
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};
    let fixture = Fixture::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!(
        "http://127.0.0.1:{}/v1/embeddings",
        listener.local_addr().unwrap().port()
    );
    let stopped = Arc::new(AtomicBool::new(false));
    let received = Arc::new(AtomicUsize::new(0));
    let server_stop = stopped.clone();
    let server_received = received.clone();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && !server_stop.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    // Darwin accept inherits the listener's nonblocking flag.
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    let mut body = Vec::new();
                    let marker = b"lease-directory-identity-probe";
                    while body.len() < 65536 {
                        let mut chunk = [0u8; 4096];
                        let count = stream.read(&mut chunk).unwrap();
                        if count == 0 {
                            break;
                        }
                        body.extend_from_slice(&chunk[..count]);
                        if let Some(end) = body.windows(4).position(|part| part == b"\r\n\r\n") {
                            let headers = String::from_utf8_lossy(&body[..end]);
                            let length = headers
                                .lines()
                                .filter_map(|line| line.split_once(':'))
                                .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                                .map(|(_, value)| value.trim().parse::<usize>().unwrap());
                            if length.is_some_and(|length| body.len() >= end + 4 + length)
                                || (headers
                                    .to_ascii_lowercase()
                                    .contains("transfer-encoding: chunked")
                                    && body[end + 4..].ends_with(b"0\r\n\r\n"))
                            {
                                break;
                            }
                        }
                    }
                    if body.windows(marker.len()).any(|part| part == marker) {
                        server_received.fetch_add(1, Ordering::SeqCst);
                    }
                    let mut vector = vec![0.0_f32; 768];
                    vector[0] = 1.0;
                    let response = serde_json::json!({"data":[{"embedding":vector}]}).to_string();
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
                    stream.flush().unwrap();
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(error) => panic!("synthetic loopback listener failed: {error}"),
            }
        }
    });
    let mut settings = (*loopback_settings(&fixture)).clone();
    settings.declarations.get_mut("embedding").unwrap().url = Some(url);
    let settings = Arc::new(settings);
    let execution =
        crate::embedding::admission::embedding_execution(&settings, fixture.repo.control())
            .unwrap();
    let profile = crate::embedding::admission::declared_embedding_profile(execution);
    fixture
        .approve_with(
            &settings,
            AdapterTarget::Tool(profile.tool_id.clone()),
            None,
            false,
        )
        .unwrap();
    let path = fixture.grant_path();
    let source = PrivateGrantStore::open_readonly(
        &path,
        &crate::settings::data_home(fixture.repo.control()),
        fixture.repo.control(),
    )
    .unwrap()
    .unwrap();
    let network = source
        .records()
        .iter()
        .find(|r| matches!(&r.binding, GrantBinding::DeviceEndpoint(_)))
        .unwrap();
    let GrantBinding::DeviceEndpoint(endpoint) = network.binding.clone() else {
        unreachable!()
    };
    let replacement_directory = path.parent().unwrap().with_file_name("replacement-grants");
    let replacement_path = replacement_directory.join("grants.json");
    let mut replacement = PrivateGrantStore::open_or_create(
        &replacement_path,
        &crate::settings::data_home(fixture.repo.control()),
        fixture.repo.control(),
    )
    .unwrap();
    let (pending, _) = replacement
        .begin_pending_endpoint(
            endpoint.clone(),
            network.disclosure.clone().unwrap(),
            None,
            "2026-10-20T00:00:00Z".into(),
        )
        .unwrap();
    replacement
        .activate_endpoint_records(&pending, None, "2026-10-20T00:01:00Z".into())
        .unwrap();
    if active_original {
        replacement
            .revoke_endpoint(&endpoint, "2026-10-20T00:02:00Z".into())
            .unwrap();
    }
    drop(replacement);
    if !active_original {
        let mut original = PrivateGrantStore::open_or_create(
            &path,
            &crate::settings::data_home(fixture.repo.control()),
            fixture.repo.control(),
        )
        .unwrap();
        original
            .revoke_endpoint(&endpoint, "2026-10-20T00:02:00Z".into())
            .unwrap();
    }
    let parked = path
        .parent()
        .unwrap()
        .with_file_name("retained-original-grants");
    let admitted = super::approvals::endpoint_dispatch_lease_with_checkpoint(
        &settings,
        &fixture.repo,
        super::approvals::AdapterRole::Embedding,
        &profile.tool_id,
        &profile.profile_hash,
        false,
        |phase| {
            if phase != super::approvals::EndpointLeasePhase::Locked {
                return Ok(());
            }
            fs::rename(path.parent().unwrap(), &parked).unwrap();
            fs::rename(&replacement_directory, path.parent().unwrap()).unwrap();
            Ok(())
        },
    );
    let was_admitted = matches!(&admitted, Ok(Some(_)));
    if let Ok(Some(lease)) = admitted {
        // A writer for replacement B is independent of the retained A lock.
        // Its completed revoke must not let B supply authority under A's lease.
        let mut replacement = PrivateGrantStore::open_or_create(
            &path,
            &crate::settings::data_home(fixture.repo.control()),
            fixture.repo.control(),
        )
        .unwrap();
        replacement
            .revoke_endpoint(&endpoint, "2026-10-20T00:03:00Z".into())
            .unwrap();
        drop(replacement);
        let completed = crate::embedding::transport::run_embedding_adapter(
            &settings,
            execution,
            vec![kio_adapter::types::EmbeddingItem::text(
                "probe",
                "lease-directory-identity-probe",
            )],
            kio_adapter::types::EmbeddingInputType::Query,
            None,
            fixture.repo.control(),
        );
        assert!(
            completed.is_ok(),
            "synthetic loopback physical embedding must complete"
        );
        drop(lease);
    }
    stopped.store(true, Ordering::SeqCst);
    server.join().unwrap();
    assert_eq!(
        received.load(Ordering::SeqCst),
        usize::from(active_original),
        "actual content dispatch must use the grant snapshot in the directory protected by its lease"
    );
    assert_eq!(
        was_admitted, active_original,
        "nominal replacement must neither supply nor withdraw retained authority"
    );
}

#[cfg(unix)]
#[test]
fn revoked_retained_grant_directory_cannot_borrow_active_replacement_for_dispatch() {
    assert_dispatch_grant_directory_binding(false);
}

#[cfg(unix)]
#[test]
fn active_retained_grant_directory_keeps_authority_when_nominal_replacement_is_revoked() {
    assert_dispatch_grant_directory_binding(true);
}

#[cfg(unix)]
fn assert_retired_child_secret_is_revalidated_under_dispatch_lease() {
    use crate::grants::{GrantBinding, PrivateGrantStore};
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};
    let fixture = Fixture::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!(
        "http://127.0.0.1:{}/v1/embeddings",
        listener.local_addr().unwrap().port()
    );
    let stopped = Arc::new(AtomicBool::new(false));
    let received = Arc::new(AtomicUsize::new(0));
    let server_stop = stopped.clone();
    let server_received = received.clone();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && !server_stop.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    // Darwin accept inherits the listener's nonblocking flag.
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    let mut body = Vec::new();
                    let marker = b"lease-directory-identity-probe";
                    while body.len() < 65536 {
                        let mut chunk = [0u8; 4096];
                        let count = stream.read(&mut chunk).unwrap();
                        if count == 0 {
                            break;
                        }
                        body.extend_from_slice(&chunk[..count]);
                        if let Some(end) = body.windows(4).position(|part| part == b"\r\n\r\n") {
                            let headers = String::from_utf8_lossy(&body[..end]);
                            let length = headers
                                .lines()
                                .filter_map(|line| line.split_once(':'))
                                .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                                .map(|(_, value)| value.trim().parse::<usize>().unwrap());
                            if length.is_some_and(|length| body.len() >= end + 4 + length)
                                || (headers
                                    .to_ascii_lowercase()
                                    .contains("transfer-encoding: chunked")
                                    && body[end + 4..].ends_with(b"0\r\n\r\n"))
                            {
                                break;
                            }
                        }
                    }
                    if body.windows(marker.len()).any(|part| part == marker) {
                        server_received.fetch_add(1, Ordering::SeqCst);
                    }
                    let mut vector = vec![0.0_f32; 768];
                    vector[0] = 1.0;
                    let response = serde_json::json!({"data":[{"embedding":vector}]}).to_string();
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
                    stream.flush().unwrap();
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(error) => panic!("synthetic loopback listener failed: {error}"),
            }
        }
    });
    let mut settings = (*loopback_settings(&fixture)).clone();
    settings.declarations.get_mut("embedding").unwrap().url = Some(url);
    let settings = Arc::new(settings);
    let execution =
        crate::embedding::admission::embedding_execution(&settings, fixture.repo.control())
            .unwrap();
    let profile = crate::embedding::admission::declared_embedding_profile(execution);
    let child_path = fixture.root.join("secret-child");
    fs::create_dir(&child_path).unwrap();
    let child =
        match crate::management::reconcile_planned_child(&fixture.repo, bound_child(&child_path))
            .unwrap()
        {
            crate::management::ChildScope::Managed { repo } => repo,
            _ => panic!("real managed child required"),
        };
    fs::write(child_path.join(".env"), "lease-directory-identity-probe").unwrap();
    let needs_secret = kio_pipeline::scan::classify_secret(".env").is_some();
    assert!(
        needs_secret,
        "positive fixture must represent scoped secret content"
    );
    fixture
        .in_context_at(&settings, &child_path, |ctx| {
            super::approvals::approve(
                ctx,
                ApproveArgs {
                    target: AdapterTarget::Tool(profile.tool_id.clone()),
                    preview: false,
                    yes: true,
                    resume: None,
                    send_secrets: true,
                },
                &super::approvals::ApprovalControl::default(),
            )
        })
        .unwrap();
    let child_lock = child.lock_store_in_session(child.lock_session()).unwrap();
    let policy = crate::search::scope::current_embedding_policy(&child).unwrap();
    crate::egress::admit_embedding_transport(
        &settings,
        &child,
        execution,
        &profile,
        &policy,
        needs_secret,
    )
    .unwrap();
    let moved = fixture.root.with_file_name("retained-secret-child");
    let admitted = super::approvals::endpoint_dispatch_lease_with_checkpoint(
        &settings,
        &child,
        super::approvals::AdapterRole::Embedding,
        &profile.tool_id,
        &profile.profile_hash,
        needs_secret,
        |phase| {
            if phase != super::approvals::EndpointLeasePhase::Configured {
                return Ok(());
            }
            fs::rename(&child_path, &moved).unwrap();
            // Real production retirement holds only the parent scope writer;
            // the sender still holds the displaced old child's writer above.
            crate::management::reconcile_child_lifecycle(&fixture.repo).unwrap();
            let snapshot = PrivateGrantStore::open_readonly(
                &fixture.grant_path(),
                &crate::settings::data_home(fixture.repo.control()),
                fixture.repo.control(),
            )
            .unwrap()
            .unwrap();
            assert!(
                snapshot.records().iter().any(|r| matches!(
                    &r.binding,
                    GrantBinding::DeviceEndpoint(_)
                ) && r.state
                    == crate::grants::GrantState::Active),
                "ordinary endpoint must remain active"
            );
            assert!(
                snapshot
                    .records()
                    .iter()
                    .any(|r| r.parent_network_grant_id.is_some()
                        && r.state == crate::grants::GrantState::Revoked),
                "retirement must complete scoped secret withdrawal"
            );
            Ok(())
        },
    );
    if let Ok(Some(lease)) = admitted {
        let completed = crate::embedding::transport::run_embedding_adapter(
            &settings,
            execution,
            vec![kio_adapter::types::EmbeddingItem::text(
                "probe",
                "lease-directory-identity-probe",
            )],
            kio_adapter::types::EmbeddingInputType::MarkdownChunk,
            None,
            fixture.repo.control(),
        );
        assert!(
            completed.is_ok(),
            "synthetic loopback physical embedding must complete"
        );
        drop(lease);
    }
    stopped.store(true, Ordering::SeqCst);
    server.join().unwrap();
    drop(child_lock);
    assert_eq!(
        received.load(Ordering::SeqCst),
        0,
        "completed parent-owned scoped-secret retirement must deny the old retained child's physical secret payload"
    );
}

#[cfg(unix)]
#[test]
fn parent_retirement_withdraws_scoped_secret_before_final_device_dispatch_lease() {
    assert_retired_child_secret_is_revalidated_under_dispatch_lease();
}

#[test]
fn final_endpoint_lease_checks_secret_pair_without_invalidating_normal_scope_authority() {
    use crate::grants::{GrantOperation, GrantState, PrivateGrantStore};
    let fixture = Fixture::new();
    let settings = loopback_settings(&fixture);
    let execution =
        crate::embedding::admission::embedding_execution(&settings, fixture.repo.control())
            .unwrap();
    let profile = crate::embedding::admission::declared_embedding_profile(execution);
    fixture
        .approve_with(
            &settings,
            AdapterTarget::Tool(profile.tool_id.clone()),
            None,
            true,
        )
        .unwrap();
    assert!(
        super::approvals::allowed(
            &settings,
            &fixture.repo,
            super::approvals::AdapterRole::Embedding,
            &profile.tool_id,
            &profile.profile_hash,
            GrantOperation::SendSecrets
        )
        .unwrap()
    );
    let scope_before = fixture.metadata_snapshot();
    let mut store = PrivateGrantStore::open_or_create(
        &fixture.grant_path(),
        &crate::settings::data_home(fixture.repo.control()),
        fixture.repo.control(),
    )
    .unwrap();
    let scope_id = fixture.repo.scope_identity().unwrap().scope_id;
    let revoked = store
        .revoke(
            &scope_id,
            Some(&profile.tool_id),
            "2026-10-20T00:00:00Z".into(),
        )
        .unwrap();
    assert_eq!(revoked.len(), 1, "withdraw only the scoped secret half");
    assert!(
        store
            .records()
            .iter()
            .any(|r| r.binding.operation() == GrantOperation::Network
                && r.state == GrantState::Active)
    );
    drop(store);
    assert_eq!(
        fixture.metadata_snapshot(),
        scope_before,
        "scope policy/reference and physical identity must remain valid"
    );
    let normal = super::approvals::endpoint_dispatch_lease(
        &settings,
        &fixture.repo,
        super::approvals::AdapterRole::Embedding,
        &profile.tool_id,
        &profile.profile_hash,
        false,
    )
    .unwrap()
    .unwrap();
    drop(normal);
    let secret = super::approvals::endpoint_dispatch_lease(
        &settings,
        &fixture.repo,
        super::approvals::AdapterRole::Embedding,
        &profile.tool_id,
        &profile.profile_hash,
        true,
    );
    assert!(
        matches!(secret, Err(ref error) if error.error_code() == "KIO-E-ADAPTER-APPROVAL-REQUIRED-001"),
        "a valid normal endpoint lease cannot substitute for withdrawn scoped secret consent"
    );
}
