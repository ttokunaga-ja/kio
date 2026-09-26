//! Device-grant contracts for external adapter permissions.

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

struct StopAfterReset;

impl StopAfterReset {
    fn set(phase: &'static str) -> Self {
        super::approvals::STOP_AFTER.with(|stop_after| stop_after.set(Some(phase)));
        Self
    }
}

impl Drop for StopAfterReset {
    fn drop(&mut self) {
        super::approvals::STOP_AFTER.with(|stop_after| stop_after.set(None));
    }
}

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
}

impl Fixture {
    fn new() -> Self {
        let lock = kio_core::test_control::test_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let temp = tempfile::tempdir().unwrap();
        // macOS commonly exposes tempfile paths below `/var`, whose canonical
        // spelling is `/private/var`. Private grant validation intentionally
        // rejects that unresolved alias, so bind every fixture path from the
        // same canonical base before assigning HOME/XDG.
        let base = temp.path().canonicalize().unwrap();
        let root = base.join("scope");
        fs::create_dir(&root).unwrap();
        let repo = match crate::management::initialize_explicit_root(&root).unwrap() {
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
        super::approvals::store_path()
    }

    fn metadata_snapshot(&self) -> Vec<(PathBuf, Vec<u8>)> {
        snapshot_tree(self.repo.kio_dir())
    }

    fn approve(&self, send_secrets: bool) -> Value {
        self.approve_with(AdapterTarget::All, None, send_secrets)
            .unwrap()
    }

    fn approve_with(
        &self,
        target: AdapterTarget,
        resume: Option<String>,
        send_secrets: bool,
    ) -> kio_core::Result<Value> {
        self.in_context(|| {
            super::approvals::approve(ApproveArgs {
                target,
                preview: false,
                yes: true,
                resume,
                send_secrets,
            })
        })
    }

    fn status(&self) -> Value {
        self.in_context(|| super::approvals::status(AdapterStatusArgs { tool_id: None }))
            .unwrap()
    }

    fn revoke(&self) -> Value {
        self.in_context(|| {
            super::approvals::revoke(RevokeArgs {
                target: AdapterTarget::Tool(TOOL_ID.to_owned()),
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

    fn network_allowed(&self, profile: &str) -> bool {
        super::persistent_network_allowed_for(
            &self.repo,
            super::approvals::AdapterRole::Markdown,
            TOOL_ID,
            profile,
        )
        .unwrap()
    }

    fn in_context<T>(
        &self,
        operation: impl FnOnce() -> kio_core::Result<T>,
    ) -> kio_core::Result<T> {
        self.in_context_at(&self.root, operation)
    }

    fn in_context_at<T>(
        &self,
        root: &Path,
        operation: impl FnOnce() -> kio_core::Result<T>,
    ) -> kio_core::Result<T> {
        let context = AppContext {
            working_directory: root.to_path_buf(),
            interaction: Arc::new(TestInteraction),
        };
        crate::context::with_context(&context, operation)
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
    let error = super::approvals::revoke(RevokeArgs {
        target: AdapterTarget::Tool("\n".to_owned()),
    })
    .expect_err("control characters are not adapter identifiers");
    assert_eq!(error.error_code(), "KIO-E-CONFIG-USAGE-001");
}

#[test]
fn preview_and_status_are_readonly_and_do_not_create_or_repair_grants() {
    let fixture = Fixture::new();
    let before = fixture.metadata_snapshot();
    assert!(!fixture.grant_path().exists());

    let preview = fixture
        .in_context(|| {
            super::approvals::approve(ApproveArgs {
                target: AdapterTarget::All,
                preview: true,
                yes: false,
                resume: None,
                send_secrets: true,
            })
        })
        .unwrap();
    assert_eq!(preview["status"], "preview");
    let status = fixture.status();
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
    for phase in [
        "device_pending",
        "scope_pending",
        "scope_config",
        "scope_published",
        "device_active",
    ] {
        let fixture = Fixture::new();
        let reset = StopAfterReset::set(phase);
        let error = fixture
            .approve_with(AdapterTarget::Tool(TOOL_ID.to_owned()), None, true)
            .unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-APPROVAL-TEST-INTERRUPTED-001");
        assert_eq!(error.context()["phase"], phase);
        drop(reset);

        let status = fixture.status();
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
                !fixture.network_allowed(&profile),
                "{phase} must not authorize before device activation"
            );
            assert!(
                !super::approvals::secrets_allowed(
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
                AdapterTarget::Tool(TOOL_ID.to_owned()),
                Some(network_id),
                true,
            )
            .unwrap();
        assert_eq!(resumed["status"], "approved");
        assert!(fixture.network_allowed(&fixture.active_profile()));
        assert!(
            super::approvals::secrets_allowed(
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
    let fixture = Fixture::new();
    let reset = StopAfterReset::set("scope_pending");
    let error = fixture
        .approve_with(AdapterTarget::Tool(TOOL_ID.to_owned()), None, true)
        .unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-APPROVAL-TEST-INTERRUPTED-001");
    drop(reset);

    let status = fixture.status();
    let network_id = network_grant_id(&status);
    let profile = network_grant_profile(&status);
    let revoked = fixture.revoke();
    assert_eq!(revoked["status"], "revoked");
    let error = fixture
        .approve_with(
            AdapterTarget::Tool(TOOL_ID.to_owned()),
            Some(network_id),
            true,
        )
        .unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-ADAPTER-APPROVAL-REQUIRED-001");
    assert!(!fixture.network_allowed(&profile));
    assert!(
        !super::approvals::secrets_allowed(
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
    let fixture = Fixture::new();
    fixture.approve(false);
    let profile = fixture.active_profile();
    assert!(fixture.network_allowed(&profile));

    let copied_root = fixture.root.parent().unwrap().join("copied-scope");
    fs::create_dir(&copied_root).unwrap();
    let copied = match crate::management::initialize_explicit_root(&copied_root).unwrap() {
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
        !super::persistent_network_allowed_for(
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
    let fixture = Fixture::new();
    fixture.approve(false);
    let profile = fixture.active_profile();
    assert!(fixture.network_allowed(&profile));
    assert!(!fixture.network_allowed("sha256:drifted-profile"));

    let old = fixture.repo.read_config_document().unwrap();
    fixture
        .repo
        .compare_replace_config_document(&old, "[adapter.policy]\nallow_network = false\n")
        .unwrap();
    assert!(!fixture.network_allowed(&profile));

    let outcome = fixture.approve(false);
    assert_eq!(outcome["status"], "approved");
    assert!(fixture.network_allowed(&profile));
}

#[test]
fn embedding_grants_require_the_explicit_current_role_and_profile() {
    use super::approvals::{AdapterRole, allowed};
    use super::grants::GrantOperation;
    use kio_adapter::tool_lock::{AdapterRuntimeSettings, DeclaredAdapter, with_runtime_settings};
    use kio_core::test_control::{DebugTestControl, GeminiEmbedMode, Selector, install_scoped};

    let fixture = Fixture::new();
    let _control = install_scoped(DebugTestControl::default());
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
    with_runtime_settings(settings, || {
        let profile = super::declared_embedding_profile(super::embedding_execution().unwrap());
        let target = AdapterTarget::Tool(profile.tool_id.clone());
        let preview = fixture
            .in_context(|| {
                super::approvals::approve(ApproveArgs {
                    target: target.clone(),
                    preview: true,
                    yes: false,
                    resume: None,
                    send_secrets: true,
                })
            })
            .unwrap();
        assert_eq!(preview["adapters"][0]["tool_id"], profile.tool_id);
        assert!(
            preview["adapters"][0].get("role").is_none(),
            "role does not alter preview schema"
        );
        fixture.approve_with(target, None, true).unwrap();
        let grants = fs::read(fixture.grant_path()).unwrap();
        let scope_approvals = fixture.repo.read_network_approvals().unwrap();
        let query = |role, requested_profile: &str, operation| {
            allowed(
                &fixture.repo,
                role,
                &profile.tool_id,
                requested_profile,
                operation,
            )
        };
        for operation in [GrantOperation::Network, GrantOperation::SendSecrets] {
            assert!(query(AdapterRole::Embedding, &profile.profile_hash, operation).unwrap());
            assert!(!query(AdapterRole::Markdown, &profile.profile_hash, operation).unwrap());
            assert!(!query(AdapterRole::Embedding, "sha256:drifted-profile", operation).unwrap());
        }
        assert!(
            fixture.status()["effective"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["permitted"] == true)
        );

        // Persisted embedding grants cannot borrow Markdown runtime authority
        // when rebuild has no active embedding adapter.
        with_runtime_settings(AdapterRuntimeSettings::default(), || {
            assert!(super::embedding_execution().is_none());
            for operation in [GrantOperation::Network, GrantOperation::SendSecrets] {
                assert!(!query(AdapterRole::Embedding, &profile.profile_hash, operation).unwrap());
            }
            assert!(
                fixture.status()["effective"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|row| row["permitted"] == false)
            );
        });
        {
            // The existing CLI rebuild regression uses this mismatched profile.
            let mut control = DebugTestControl::default();
            control.adapters.gemini_embed = Selector::Known(GeminiEmbedMode::IncompatibleProfile);
            let _drift = install_scoped(control);
            let current = super::declared_embedding_profile(super::embedding_execution().unwrap());
            assert_ne!(current.profile_hash, profile.profile_hash);
            for operation in [GrantOperation::Network, GrantOperation::SendSecrets] {
                assert!(!query(AdapterRole::Embedding, &profile.profile_hash, operation).unwrap());
            }
        }
        // Querying absence/drift never rewrites grants; restoring the exact
        // currently configured identity restores its existing exact approval.
        assert!(
            query(
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
    });
}

#[test]
fn approval_query_preserves_current_runtime_and_config_errors() {
    use super::approvals::{AdapterRole, allowed};
    use super::grants::GrantOperation;
    use kio_adapter::tool_lock::{AdapterRuntimeSettings, DeclaredAdapter, with_runtime_settings};
    use kio_core::test_control::{DebugTestControl, install_scoped};

    let fixture = Fixture::new();
    let _control = install_scoped(DebugTestControl::default());
    fixture.approve(true);
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
    with_runtime_settings(malformed, || {
        assert!(
            allowed(
                &fixture.repo,
                AdapterRole::Markdown,
                TOOL_ID,
                &profile,
                GrantOperation::SendSecrets
            )
            .is_err()
        );
    });
    let before = fixture.repo.read_config_document().unwrap();
    // Simulate external corruption; the production replacement API correctly
    // refuses malformed config before it can be written.
    fs::write(fixture.repo.kio_dir().join("config.toml"), "[invalid").unwrap();
    assert!(
        allowed(
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
    let fixture = Fixture::new();
    fixture.approve(false);
    let profile = fixture.active_profile();
    assert!(fixture.network_allowed(&profile));
    assert!(
        !super::approvals::secrets_allowed(
            &fixture.repo,
            super::approvals::AdapterRole::Markdown,
            TOOL_ID,
            &profile
        )
        .unwrap()
    );

    fixture.approve(true);
    assert!(fixture.network_allowed(&fixture.active_profile()));
    assert!(
        super::approvals::secrets_allowed(
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
    let fixture = Fixture::new();
    fixture.approve(true);
    let profile = fixture.active_profile();
    let before = crate::management::binding_for_repo(&fixture.repo).unwrap();
    let generation = registration_generation(&fixture.repo);
    let grant_bytes = fs::read(fixture.grant_path()).unwrap();
    let scope_rows = fixture.repo.read_network_approvals().unwrap();
    assert!(fixture.network_allowed(&profile));
    assert!(
        super::approvals::secrets_allowed(
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
    assert!(!fixture.network_allowed(&profile));
    assert!(
        !super::approvals::secrets_allowed(
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
        .in_context_at(&child_path, || {
            super::approvals::approve(ApproveArgs {
                target: AdapterTarget::Tool(TOOL_ID.to_owned()),
                preview: false,
                yes: true,
                resume: None,
                send_secrets: true,
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
        super::persistent_network_allowed_for(
            &child,
            super::approvals::AdapterRole::Markdown,
            TOOL_ID,
            &profile
        )
        .unwrap()
    );
    assert!(
        super::approvals::secrets_allowed(
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
        !super::persistent_network_allowed_for(
            &child,
            super::approvals::AdapterRole::Markdown,
            TOOL_ID,
            &profile
        )
        .unwrap()
    );
    assert!(
        !super::approvals::secrets_allowed(
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
    let fixture = Fixture::new();
    let reset = StopAfterReset::set("scope_pending");
    let error = fixture
        .approve_with(AdapterTarget::Tool(TOOL_ID.to_owned()), None, true)
        .unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-APPROVAL-TEST-INTERRUPTED-001");
    drop(reset);
    let pending = network_grant_id(&fixture.status());
    let grant_bytes = fs::read(fixture.grant_path()).unwrap();
    let pending_scope = fixture.repo.read_network_approval_pending().unwrap();

    advance_registration_generation(&fixture.repo);
    assert_eq!(fs::read(fixture.grant_path()).unwrap(), grant_bytes);
    assert_eq!(
        fixture.repo.read_network_approval_pending().unwrap(),
        pending_scope
    );

    let error = fixture
        .approve_with(AdapterTarget::Tool(TOOL_ID.to_owned()), Some(pending), true)
        .unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-ADAPTER-APPROVAL-REQUIRED-001");
}
