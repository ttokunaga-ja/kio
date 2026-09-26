//! Explicit device grants paired with the current scope's approval references.

use std::path::{Path, PathBuf};

use kio_adapter::authority::{RuntimeExecutionIdentityRequest, runtime_execution_identity};
use kio_core::cas::{canonical_json_bytes, hash_bytes};
use kio_core::management::validate_live_chain;
use kio_core::scope::Repository;
use kio_core::{ExitCode, KioError, Result};
use kio_pipeline::policy::CurrentPolicyEvaluator;
use serde_json::{Value, json};

use crate::commands::{AdapterStatusArgs, AdapterTarget, ApproveArgs, RevokeArgs};
use crate::grants::{GrantBinding, GrantOperation, GrantRecord, GrantState, PrivateGrantStore};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdapterRole {
    Markdown,
    Embedding,
}

impl AdapterRole {
    fn as_str(self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::Embedding => "embedding",
        }
    }
}

pub(crate) fn store_path() -> PathBuf {
    super::data_home().join("kio/grants/grants.json")
}

fn selected_tool(target: &AdapterTarget) -> Result<Option<&str>> {
    match target {
        AdapterTarget::All => Ok(None),
        AdapterTarget::Tool(tool) if !tool.is_empty() && !tool.chars().any(char::is_control) => {
            Ok(Some(tool))
        }
        _ => Err(KioError::invalid_usage(
            "adapter target must be a nonempty tool ID",
        )),
    }
}

/// Membership excludes sibling enrollment and mutable policy content. Current
/// policy is independently reloaded at every admission; neither a new sibling
/// nor this command's config update must silently invalidate an unrelated grant.
fn binding(
    repo: &Repository,
    role: AdapterRole,
    tool_id: &str,
    profile: &str,
    operation: GrantOperation,
) -> Result<GrantBinding> {
    let retained = super::binding_for_repo(repo)?;
    let chain = validate_live_chain(&retained)?;
    let current = chain.scopes.first().expect("validated chain is nonempty");
    let policy = CurrentPolicyEvaluator::load(&retained, current.record.case_insensitive)
        .map_err(super::pipeline_to_kio)?;
    if !policy.allows_scope().map_err(super::pipeline_to_kio)? {
        return Err(denied("scope is excluded by its current management policy"));
    }
    let memberships = chain
        .scopes
        .iter()
        .map(|scope| {
            json!({
                "scope_id": scope.record.scope_id,
                "registration_generation": scope.record.registration_generation,
                "canonical_root": scope.record.canonical_root,
                "directory_identity": scope.record.directory_identity,
                "case_insensitive": scope.record.case_insensitive,
                "authority": scope.record.authority,
            })
        })
        .collect::<Vec<_>>();
    let identity = runtime_execution_identity(RuntimeExecutionIdentityRequest {
        role: role.as_str(),
        tool_id,
        tool_profile_hash: profile,
    })
    .map_err(super::adapter_to_kio)?;
    policy.revalidate().map_err(super::pipeline_to_kio)?;
    Ok(GrantBinding {
        scope_id: current.record.scope_id.clone(),
        canonical_root: retained.canonical_root().to_path_buf(),
        directory_identity: retained.directory_identity().clone(),
        membership_hash: hash_bytes(&canonical_json_bytes(&json!(memberships))?),
        tool_id: identity.tool_id,
        execution_mode: identity.execution_mode,
        tool_profile_hash: identity.tool_profile_hash,
        destination: identity.canonical_destination,
        credential_binding: identity.credential_binding,
        trust_binding: identity.trust_binding,
        operation,
    })
}

fn configured_profile(repo: &Repository, role: AdapterRole) -> Result<Option<(String, String)>> {
    // Validate the declared role before its optional execution resolver: an
    // invalid declaration must remain an error, not look like an absent lane.
    if let Some(declared) = kio_adapter::tool_lock::registered_declared_adapter(role.as_str()) {
        kio_adapter::tool_lock::validate_declared_runtime_target(role.as_str(), &declared)
            .map_err(super::adapter_to_kio)?;
    }
    let (tool_id, profile) = match role {
        AdapterRole::Markdown => {
            let profile = match super::active_local_ocr_execution() {
                Some(execution) => kio_adapter::local_ocr_markdownize::profile_for(execution),
                None => super::online_markdownize_profile_for(repo)?,
            };
            (profile.adapter_id, profile.tool_profile_hash)
        }
        AdapterRole::Embedding => {
            let Some(execution) = super::embedding_execution() else {
                return Ok(None);
            };
            // A library has no external recipient to approve.
            if super::embedding_is_deterministic(execution) {
                return Ok(None);
            }
            let profile = super::declared_embedding_profile(execution);
            (profile.tool_id, profile.profile_hash)
        }
    };
    Ok(Some((tool_id, profile)))
}

fn configured_binding(
    repo: &Repository,
    role: AdapterRole,
    operation: GrantOperation,
) -> Result<Option<GrantBinding>> {
    let Some((tool_id, profile)) = configured_profile(repo, role)? else {
        return Ok(None);
    };
    binding(repo, role, &tool_id, &profile, operation).map(Some)
}

fn configured_bindings(repo: &Repository) -> Result<Vec<(AdapterRole, GrantBinding)>> {
    let mut bindings = Vec::new();
    for role in [AdapterRole::Markdown, AdapterRole::Embedding] {
        if let Some(binding) = configured_binding(repo, role, GrantOperation::Network)? {
            bindings.push((role, binding));
        }
    }
    bindings.sort_by(|a, b| a.1.tool_id.cmp(&b.1.tool_id));
    Ok(bindings)
}

/// Admission for an explicitly online index invocation. Individual adapters
/// still require their own exact grant at dispatch: an embedding grant must
/// neither authorize Markdown conversion nor require that unrelated grant.
pub(crate) fn any_network_allowed(repo: &Repository) -> Result<bool> {
    for (role, binding) in configured_bindings(repo)? {
        if allowed(
            repo,
            role,
            &binding.tool_id,
            &binding.tool_profile_hash,
            GrantOperation::Network,
        )? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn matching_record<'a>(
    records: &'a [GrantRecord],
    row: &Value,
    expected: &GrantBinding,
) -> Option<&'a GrantRecord> {
    if row.get("status").and_then(Value::as_str) != Some("active")
        || row.get("scope_id").and_then(Value::as_str) != Some(expected.scope_id.as_str())
        || row.get("tool_id").and_then(Value::as_str) != Some(expected.tool_id.as_str())
        || row.get("execution_mode").and_then(Value::as_str)
            != Some(expected.execution_mode.as_str())
        || row.get("tool_profile_hash").and_then(Value::as_str)
            != Some(expected.tool_profile_hash.as_str())
    {
        return None;
    }
    let leaf = match expected.operation {
        GrantOperation::Network => "grant_id",
        GrantOperation::SendSecrets => "secrets_grant_id",
    };
    let id = row.get(leaf)?.as_str()?;
    records.iter().find(|record| {
        record.grant_id == id && record.state == GrantState::Active && &record.binding == expected
    })
}

pub(crate) fn allowed(
    repo: &Repository,
    role: AdapterRole,
    tool_id: &str,
    profile: &str,
    operation: GrantOperation,
) -> Result<bool> {
    let Some(snapshot) = PrivateGrantStore::open_readonly(&store_path())? else {
        return Ok(false);
    };
    let rows = repo.read_network_approvals()?;
    // An absent scope reference cannot be repaired from the device store.
    if !rows.iter().any(|row| {
        row.get("tool_id").and_then(Value::as_str) == Some(tool_id)
            && row.get("status").and_then(Value::as_str) == Some("active")
    }) {
        return Ok(false);
    }
    let Some((current_tool, current_profile)) = configured_profile(repo, role)? else {
        return Ok(false);
    };
    // Persisted output identity is not current execution authority. In
    // particular, rebuild may replay old public vectors with no active lane.
    // Do not resolve an unrelated recipient when the requested identity drifted.
    if current_tool != tool_id || current_profile != profile {
        return Ok(false);
    }
    let expected = binding(repo, role, tool_id, profile, operation)?;
    if expected.execution_mode == "online_api" {
        let document = repo.read_config_document()?;
        let config: toml::Value = toml::from_str(&document)
            .map_err(|_| KioError::schema("scope configuration is invalid"))?;
        if config
            .get("adapter")
            .and_then(|v| v.get("policy"))
            .and_then(|v| v.get("allow_network"))
            .and_then(toml::Value::as_bool)
            == Some(false)
        {
            return Ok(false);
        }
    }
    Ok(rows
        .iter()
        .any(|row| matching_record(snapshot.records(), row, &expected).is_some()))
}

pub(crate) fn allowed_at(
    kio_dir: &Path,
    role: AdapterRole,
    tool_id: &str,
    profile: &str,
    operation: GrantOperation,
) -> Result<bool> {
    let root = kio_dir
        .parent()
        .ok_or_else(|| KioError::invalid_usage("scope store has no parent"))?;
    let repo = super::management::open_existing_managed_root(root)?;
    allowed(&repo, role, tool_id, profile, operation)
}

/// Whether this exact external recipient may receive secret content. A grant
/// for another configured adapter is neither required nor sufficient.
pub(crate) fn secrets_allowed(
    repo: &Repository,
    role: AdapterRole,
    tool_id: &str,
    profile: &str,
) -> Result<bool> {
    allowed(repo, role, tool_id, profile, GrantOperation::SendSecrets)
}

fn open_network_policy(repo: &Repository) -> Result<()> {
    let old = repo.read_config_document()?;
    let mut document = old
        .parse::<toml_edit::DocumentMut>()
        .map_err(|_| KioError::schema("scope configuration is invalid"))?;
    document["adapter"]["policy"]["allow_network"] = toml_edit::value(true);
    repo.compare_replace_config_document(&old, &document.to_string())
}

pub(crate) fn approve(args: ApproveArgs) -> Result<Value> {
    let tool = selected_tool(&args.target)?;
    if args.preview && args.yes || args.resume.is_some() && tool.is_none() {
        return Err(KioError::invalid_usage(
            "preview conflicts with --yes; --resume requires one adapter",
        ));
    }
    let repo = super::open_current_recovery_repository()?;
    let selected = configured_bindings(&repo)?
        .into_iter()
        .filter(|(_, b)| tool.is_none_or(|tool| b.tool_id == tool))
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return Err(KioError::invalid_usage(
            "selected adapter is not configured",
        ));
    }
    let preview = json!({"status":"preview", "scope_id":repo.scope_identity()?.scope_id,
        "adapters":selected.iter().map(|(_, binding)| binding).collect::<Vec<_>>(), "send_secrets":args.send_secrets, "resume":args.resume});
    if args.preview {
        return Ok(preview);
    }
    if !args.yes
        && !crate::context::confirm(&format!(
            "Approve these adapter permissions? [y/N]\n{preview}"
        ))?
    {
        return Err(denied(
            "approval requires confirmation; inspect --preview then use --yes or an interactive terminal",
        ));
    }
    // Lock order is scope then device. Sends already hold scope writer leases;
    // device grant publication never acquires an additional scope lease.
    let _lock = repo.lock_store()?;
    let refreshed = configured_bindings(&repo)?
        .into_iter()
        .filter(|(_, b)| tool.is_none_or(|tool| b.tool_id == tool))
        .collect::<Vec<_>>();
    if refreshed != selected {
        return Err(denied("adapter identity changed after confirmation"));
    }
    let mut store = PrivateGrantStore::open_or_create(&store_path())?;
    let mut outcomes = Vec::new();
    for (role, expected) in selected {
        let rows = repo.read_network_approvals()?;
        let row = rows
            .iter()
            .find(|row| row.get("tool_id").and_then(Value::as_str) == Some(&expected.tool_id));
        let mut secret_binding = expected.clone();
        secret_binding.operation = GrantOperation::SendSecrets;
        if args.resume.is_none()
            && row.is_some_and(|row| {
                matching_record(store.records(), row, &expected).is_some()
                    && (!args.send_secrets
                        || matching_record(store.records(), row, &secret_binding).is_some())
            })
        {
            if expected.execution_mode == "online_api" {
                open_network_policy(&repo)?;
            }
            outcomes.push(json!({"tool_id":expected.tool_id,"status":"already_approved"}));
            continue;
        }
        let now = super::now_utc_seconds();
        let current_intent = repo.read_network_approval_pending()?;
        if let Some(intent) = &current_intent
            && intent.get("grant_id").and_then(Value::as_str) != args.resume.as_deref()
        {
            return Err(denied(
                "another approval is pending; inspect adapter status and revoke or resume it",
            ));
        }
        let (pending, secret) = if let Some(id) = args.resume.as_deref() {
            let pending = store
                .records()
                .iter()
                .find(|r| r.grant_id == id)
                .filter(|r| r.state != GrantState::Revoked && r.binding == expected)
                .ok_or_else(|| {
                    denied("grant intent is revoked or does not match the current adapter identity")
                })?
                .clone();
            let secret = store
                .records()
                .iter()
                .find(|r| {
                    r.approval_id == pending.approval_id
                        && r.binding.operation == GrantOperation::SendSecrets
                })
                .cloned();
            if secret.is_some() != args.send_secrets
                || secret
                    .as_ref()
                    .is_some_and(|r| r.state == GrantState::Revoked || r.binding != secret_binding)
            {
                return Err(denied(
                    "resume must preserve the original secret-send selection and live grant identity",
                ));
            }
            if pending.state == GrantState::Active {
                if row.is_some_and(|row| {
                    matching_record(store.records(), row, &expected).is_some()
                        && secret.as_ref().is_none_or(|_| {
                            matching_record(store.records(), row, &secret_binding).is_some()
                        })
                }) {
                    outcomes.push(json!({"tool_id":expected.tool_id,"status":"already_approved","grant_id":pending.grant_id}));
                    continue;
                }
                return Err(denied(
                    "an active device grant cannot repair a missing or revoked scope reference",
                ));
            }
            if row.is_some_and(|row| {
                let id_matches = row.get("grant_id").and_then(Value::as_str) == Some(id);
                (id_matches && row.get("status").and_then(Value::as_str) != Some("active"))
                    || (!id_matches && row.get("status").and_then(Value::as_str) == Some("active"))
            }) {
                return Err(denied(
                    "another grant replaced or revoked this pending approval",
                ));
            }
            (pending, secret)
        } else {
            store.revoke(&expected.scope_id, Some(&expected.tool_id), now.clone())?;
            store.begin_pending_approval(expected.clone(), args.send_secrets, now.clone())?
        };
        approval_checkpoint("device_pending")?;
        let mut intent = json!({"scope_id":expected.scope_id,"tool_id":expected.tool_id,
            "execution_mode":expected.execution_mode,"tool_profile_hash":expected.tool_profile_hash,
            "grant_id":pending.grant_id,"approved_at":pending.created_at,"approval_method":"approve"});
        if let Some(secret) = &secret {
            intent["secrets_grant_id"] = json!(secret.grant_id);
        }
        if current_intent
            .as_ref()
            .is_some_and(|current| current != &intent)
        {
            return Err(denied(
                "scope intent does not match the complete pending device approval",
            ));
        }
        repo.write_network_approval_pending(intent.clone())?;
        approval_checkpoint("scope_pending")?;
        if expected.execution_mode == "online_api" {
            open_network_policy(&repo)?;
        }
        approval_checkpoint("scope_config")?;
        let mut published = intent.clone();
        published["status"] = json!("active");
        repo.publish_network_approval(published, Some(&intent))?;
        approval_checkpoint("scope_published")?;
        if configured_binding(&repo, role, GrantOperation::Network)?.as_ref() != Some(&expected) {
            return Err(denied(
                "adapter authority changed during approval publication",
            ));
        }
        store.activate_approval(&pending, secret.as_ref(), now)?;
        approval_checkpoint("device_active")?;
        outcomes.push(
            json!({"tool_id":expected.tool_id,"status":"approved","grant_id":pending.grant_id,
            "secrets_grant_id":secret.map(|record|record.grant_id)}),
        );
    }
    Ok(json!({"status":"approved","approvals":outcomes}))
}

pub(crate) fn revoke(args: RevokeArgs) -> Result<Value> {
    let tool = selected_tool(&args.target)?;
    let repo = super::open_current_recovery_repository()?;
    let _lock = repo.lock_store()?;
    let scope_id = repo.scope_identity()?.scope_id;
    let now = super::now_utc_seconds();
    let revoked_grants = if PrivateGrantStore::open_readonly(&store_path())?.is_some() {
        PrivateGrantStore::open_or_create(&store_path())?.revoke(&scope_id, tool, now.clone())?
    } else {
        Vec::new()
    };
    let outcome = repo.revoke_network_approval(tool, &now)?;
    Ok(
        json!({"status":if outcome.changed() || !revoked_grants.is_empty() {"revoked"} else {"no_target"},
        "tool_id":tool,"all":tool.is_none(),"revoked_tool_ids":outcome.revoked_tool_ids,
        "revoked_grant_ids":revoked_grants,"pending_removed":outcome.pending_removed}),
    )
}

pub(crate) fn status(args: AdapterStatusArgs) -> Result<Value> {
    let repo = super::open_current_recovery_repository()?;
    let scope_id = repo.scope_identity()?.scope_id;
    let snapshot = PrivateGrantStore::open_readonly(&store_path())?;
    let records = snapshot
        .as_ref()
        .map(|snapshot| snapshot.records())
        .unwrap_or_default()
        .iter()
        .filter(|record| {
            record.binding.scope_id == scope_id
                && args
                    .tool_id
                    .as_ref()
                    .is_none_or(|tool| record.binding.tool_id == *tool)
        })
        .collect::<Vec<_>>();
    let rows = repo
        .read_network_approvals()?
        .into_iter()
        .filter(|row| {
            args.tool_id
                .as_ref()
                .is_none_or(|id| row.get("tool_id").and_then(Value::as_str) == Some(id))
        })
        .collect::<Vec<_>>();
    let pending = repo.read_network_approval_pending()?.filter(|row| {
        args.tool_id
            .as_ref()
            .is_none_or(|id| row.get("tool_id").and_then(Value::as_str) == Some(id))
    });
    let mut effective = Vec::new();
    let configured = configured_bindings(&repo);
    for record in &records {
        // Roles are carried by current configuration, never guessed from a
        // historical grant's tool ID. Unsupported/absent identities stay inert.
        let check = match &configured {
            Ok(bindings) => match bindings.iter().find(|(_, binding)| {
                binding.tool_id == record.binding.tool_id
                    && binding.tool_profile_hash == record.binding.tool_profile_hash
            }) {
                Some((role, _)) => allowed(
                    &repo,
                    *role,
                    &record.binding.tool_id,
                    &record.binding.tool_profile_hash,
                    record.binding.operation,
                )
                .map_err(|error| error.error_code().to_owned()),
                None => Ok(false),
            },
            Err(error) => Err(error.error_code().to_owned()),
        };
        let (permitted, reason) = match check {
            Ok(true)
                if record.state == GrantState::Active
                    && rows.iter().any(|row| {
                        matching_record(&[(*record).clone()], row, &record.binding).is_some()
                    }) =>
            {
                (true, None)
            }
            Ok(_) => (
                false,
                Some("current_scope_or_device_binding_does_not_match".to_owned()),
            ),
            Err(error_code) => (false, Some(error_code)),
        };
        effective.push(json!({"grant_id":record.grant_id, "permitted":permitted, "reason":reason}));
    }
    Ok(
        json!({"scope_id":scope_id,"grants":records,"effective":effective,
        "scope_approvals":rows,"pending":pending}),
    )
}

fn denied(message: &str) -> KioError {
    KioError::new(
        "KIO-E-ADAPTER-APPROVAL-REQUIRED-001",
        message,
        json!({}),
        ExitCode::AuthError,
    )
}

#[cfg(test)]
thread_local! {
    pub(crate) static STOP_AFTER: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}

fn approval_checkpoint(phase: &'static str) -> Result<()> {
    #[cfg(test)]
    if STOP_AFTER.get() == Some(phase) {
        return Err(KioError::new(
            "KIO-E-APPROVAL-TEST-INTERRUPTED-001",
            "approval interrupted at a durable boundary",
            json!({"phase":phase}),
            ExitCode::PartialFailure,
        ));
    }
    let _ = phase;
    Ok(())
}
