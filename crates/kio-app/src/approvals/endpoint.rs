use super::*;
use crate::grants::{DeviceEndpointBinding, EndpointDisclosure};

pub(super) fn configured(
    settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    role: AdapterRole,
    tool: &str,
    profile: &str,
) -> Result<Option<DeviceEndpointBinding>> {
    let Some(declared) = settings.declared_adapter(role.as_str()) else {
        return Ok(None);
    };
    if declared.kind.as_deref() != Some("offline_api") {
        return Ok(None);
    }
    if configured_profile(settings, repo, role)?.as_ref()
        != Some(&(tool.to_owned(), profile.to_owned()))
    {
        return Ok(None);
    }
    validate_policy(repo)?;
    let identity = runtime_execution_identity(
        settings,
        RuntimeExecutionIdentityRequest {
            role: role.as_str(),
            tool_id: tool,
            tool_profile_hash: profile,
        },
    )
    .map_err(crate::object_persistence::adapter_to_kio)?;
    Ok(Some(DeviceEndpointBinding {
        destination: identity.canonical_destination,
        credential_binding: identity.credential_binding,
        trust_binding: identity.trust_binding,
    }))
}

fn validate_policy(repo: &Repository) -> Result<()> {
    let retained = crate::management::binding_for_repo(repo)?;
    let chain = validate_live_chain(&retained)?;
    let current = chain.scopes.first().expect("validated chain is nonempty");
    let policy = CurrentPolicyEvaluator::load(&retained, current.record.case_insensitive)
        .map_err(crate::object_persistence::pipeline_to_kio)?;
    if !policy
        .allows_scope()
        .map_err(crate::object_persistence::pipeline_to_kio)?
    {
        return Err(denied("scope is excluded by its current management policy"));
    }
    policy
        .revalidate()
        .map_err(crate::object_persistence::pipeline_to_kio)
}

fn disclosure(
    role: AdapterRole,
    tool: &str,
    profile: &str,
    endpoint: &DeviceEndpointBinding,
    secrets: bool,
) -> EndpointDisclosure {
    EndpointDisclosure {
        tool_id: tool.to_owned(),
        tool_profile_hash: profile.to_owned(),
        purpose: role.as_str().to_owned(),
        content_types: match role {
            AdapterRole::Embedding => vec!["query text", "document text", "images"],
            AdapterRole::Markdown => vec!["document page images", "document text"],
        }
        .into_iter()
        .map(str::to_owned)
        .collect(),
        applies_to: "all scopes on this device".into(),
        peer_authentication: if endpoint.destination.starts_with("http://") {
            "unauthenticated"
        } else {
            "managed_ca"
        }
        .into(),
        send_secrets: secrets,
    }
}

pub(super) fn active<'a>(
    records: &'a [GrantRecord],
    binding: &DeviceEndpointBinding,
) -> Option<&'a GrantRecord> {
    records.iter().find(|record| {
        record.state == GrantState::Active
            && record.binding == GrantBinding::DeviceEndpoint(binding.clone())
    })
}

pub(super) fn current_bindings(
    settings: &Arc<AdapterRuntimeSettings>,
    selected: Option<&str>,
) -> Result<Vec<DeviceEndpointBinding>> {
    let mut bindings = Vec::new();
    for role in [AdapterRole::Markdown, AdapterRole::Embedding] {
        let Some(declared) = settings.declared_adapter(role.as_str()) else {
            continue;
        };
        if declared.kind.as_deref() != Some("offline_api") {
            continue;
        }
        let tool = declared.tool_id.as_deref().unwrap_or(match role {
            AdapterRole::Markdown => "paddleocr_vl_local",
            AdapterRole::Embedding => "qwen3_vl_embedding_local",
        });
        if selected.is_some_and(|selected| selected != tool) {
            continue;
        }
        let (tool, profile) = match role {
            AdapterRole::Markdown => {
                let Some(execution) = kio_adapter::catalog::active_local_ocr_execution_with_control(
                    settings,
                    &settings.control,
                ) else {
                    continue;
                };
                let profile = kio_adapter::local_ocr_markdownize::profile_for(execution);
                (profile.adapter_id, profile.tool_profile_hash)
            }
            AdapterRole::Embedding => {
                let Some(execution) =
                    crate::embedding::admission::embedding_execution(settings, &settings.control)
                else {
                    continue;
                };
                let profile = crate::embedding::admission::declared_embedding_profile(execution);
                (profile.tool_id, profile.profile_hash)
            }
        };
        let identity = runtime_execution_identity(
            settings,
            RuntimeExecutionIdentityRequest {
                role: role.as_str(),
                tool_id: &tool,
                tool_profile_hash: &profile,
            },
        )
        .map_err(crate::object_persistence::adapter_to_kio)?;
        bindings.push(DeviceEndpointBinding {
            destination: identity.canonical_destination,
            credential_binding: identity.credential_binding,
            trust_binding: identity.trust_binding,
        });
    }
    Ok(bindings)
}

pub(super) fn approve(
    ctx: &crate::invocation::Invocation<'_>,
    repo: &Repository,
    role: AdapterRole,
    tool: &str,
    profile: &str,
    args: &ApproveArgs,
    control: &ApprovalControl,
) -> Result<CommandResponse> {
    let expected = configured(ctx.settings, repo, role, tool, profile)?
        .ok_or_else(|| denied("selected endpoint is no longer configured"))?;
    if expected.destination.starts_with("https://") {
        ctx.settings
            .local_endpoint(role.as_str(), &expected.destination)
            .map_err(crate::object_persistence::adapter_to_kio)?;
    }
    let disclosed = disclosure(role, tool, profile, &expected, args.send_secrets);
    let secret_binding = args
        .send_secrets
        .then(|| {
            binding(
                ctx.settings,
                repo,
                role,
                tool,
                profile,
                GrantOperation::SendSecrets,
            )
        })
        .transpose()?;
    let preview = ApprovalPreviewResponse {
        status: "preview".into(),
        scope_id: repo.scope_identity()?.scope_id,
        adapters: Vec::new(),
        device_endpoints: vec![EndpointPreview {
            peer_authenticated: expected.destination.starts_with("https://"),
            binding: expected.clone(),
            disclosure: disclosed.clone(),
            secret_scope: secret_binding.clone(),
        }],
        send_secrets: args.send_secrets,
        resume: args.resume.clone(),
    };
    if args.preview {
        return Ok(CommandResponse {
            body: ResponseBody::Adapter(AdapterResponse::ApprovalPreview(preview)),
            completion: Completion::Complete,
        });
    }
    let snapshot = PrivateGrantStore::open_readonly(
        &store_path(ctx.control),
        &crate::settings::data_home(ctx.control),
        ctx.control,
    )?;
    if args.resume.is_none()
        && snapshot
            .as_ref()
            .is_some_and(|s| active(s.records(), &expected).is_some())
        && (!args.send_secrets
            || allowed(
                ctx.settings,
                repo,
                role,
                tool,
                profile,
                GrantOperation::SendSecrets,
            )?)
    {
        return approved(tool, None, None);
    }
    if !args.yes
        && !ctx.interaction.confirm(&format!(
            "Approve these endpoint permissions? [y/N]\n{}",
            serde_json::to_value(&preview).map_err(|e| KioError::schema(e.to_string()))?
        ))?
    {
        return Err(denied("endpoint approval requires explicit confirmation"));
    }
    let _scope_lock = repo.lock_store_in_session(repo.lock_session())?;
    if configured(ctx.settings, repo, role, tool, profile)?.as_ref() != Some(&expected) {
        return Err(denied("endpoint identity changed after confirmation"));
    }
    if let Some(expected_secret) = &secret_binding
        && binding(
            ctx.settings,
            repo,
            role,
            tool,
            profile,
            GrantOperation::SendSecrets,
        )? != *expected_secret
    {
        return Err(denied("scoped secret identity changed after confirmation"));
    }
    let mut store = PrivateGrantStore::open_or_create(
        &store_path(ctx.control),
        &crate::settings::data_home(ctx.control),
        ctx.control,
    )?;
    let now = ctx.control.now_utc_seconds();
    let (pending, secret, parent) = if let Some(id) = &args.resume {
        let pending = store
            .records()
            .iter()
            .find(|r| &r.grant_id == id && r.state != GrantState::Revoked)
            .ok_or_else(|| denied("endpoint confirmation is missing or revoked"))?
            .clone();
        let parent = if let Some(parent) = &pending.parent_network_grant_id {
            store
                .records()
                .iter()
                .find(|r| {
                    &r.grant_id == parent
                        && r.state == GrantState::Active
                        && r.binding == GrantBinding::DeviceEndpoint(expected.clone())
                })
                .ok_or_else(|| denied("pending secret endpoint parent changed or was revoked"))?
                .clone()
        } else {
            pending.clone()
        };
        if parent.binding != GrantBinding::DeviceEndpoint(expected.clone())
            || pending.disclosure.as_ref() != Some(&disclosed)
        {
            return Err(denied(
                "resume must preserve endpoint identity and original disclosure",
            ));
        }
        if pending.parent_network_grant_id.is_some() {
            if pending.binding
                != GrantBinding::Scoped(
                    secret_binding
                        .clone()
                        .ok_or_else(|| denied("resume changed secret-send selection"))?,
                )
            {
                return Err(denied("pending scoped secret authority changed"));
            }
            (pending, None, parent)
        } else {
            let secret = store
                .records()
                .iter()
                .find(|r| {
                    r.approval_id == pending.approval_id && r.parent_network_grant_id.is_some()
                })
                .cloned();
            if secret.as_ref().map(|r| &r.binding)
                != secret_binding
                    .as_ref()
                    .map(|b| GrantBinding::Scoped(b.clone()))
                    .as_ref()
            {
                return Err(denied("resume changed scoped secret authority"));
            }
            (pending, secret, parent)
        }
    } else if let Some(parent) = active(store.records(), &expected).cloned() {
        let secret = secret_binding
            .clone()
            .ok_or_else(|| denied("endpoint is already active"))?;
        store.revoke(&secret.scope_id, Some(tool), now.clone())?;
        let pending =
            store.begin_pending_endpoint_secret(&parent, secret, disclosed, now.clone())?;
        (pending, None, parent)
    } else {
        let (network, secret) = store.begin_pending_endpoint(
            expected.clone(),
            disclosed,
            secret_binding.clone(),
            now.clone(),
        )?;
        (network.clone(), secret, network)
    };
    if pending.state == GrantState::Active {
        if allowed(
            ctx.settings,
            repo,
            role,
            tool,
            profile,
            GrantOperation::Network,
        )? && (!args.send_secrets
            || allowed(
                ctx.settings,
                repo,
                role,
                tool,
                profile,
                GrantOperation::SendSecrets,
            )?)
        {
            return approved(tool, Some(pending.grant_id), secret.map(|r| r.grant_id));
        }
        return Err(denied(
            "active confirmation cannot repair missing scoped consent",
        ));
    }
    approval_checkpoint(control, "device_pending")?;
    let scoped_secret = if pending.parent_network_grant_id.is_some() {
        Some(&pending)
    } else {
        secret.as_ref()
    };
    if let Some(secret) = scoped_secret {
        let intent = json!({"scope_id":repo.scope_identity()?.scope_id,"tool_id":tool,"execution_mode":"offline_api",
            "tool_profile_hash":profile,"grant_id":parent.grant_id,"secrets_grant_id":secret.grant_id,
            "approved_at":secret.created_at,"approval_method":"approve"});
        if repo
            .read_network_approval_pending()?
            .is_some_and(|old| old != intent)
        {
            return Err(denied("another scope confirmation is pending"));
        }
        repo.write_network_approval_pending(intent.clone())?;
        approval_checkpoint(control, "scope_pending")?;
        let mut published = intent.clone();
        published["status"] = json!("active");
        repo.publish_network_approval(published, Some(&intent))?;
        approval_checkpoint(control, "scope_published")?;
    }
    if configured(ctx.settings, repo, role, tool, profile)?.as_ref() != Some(&expected) {
        return Err(denied("endpoint changed during publication"));
    }
    if let Some(expected_secret) = &secret_binding
        && binding(
            ctx.settings,
            repo,
            role,
            tool,
            profile,
            GrantOperation::SendSecrets,
        )? != *expected_secret
    {
        return Err(denied("scoped secret authority changed during publication"));
    }
    store.activate_endpoint_records(&pending, secret.as_ref(), now)?;
    approval_checkpoint(control, "device_active")?;
    approved(
        tool,
        Some(parent.grant_id),
        scoped_secret.map(|r| r.grant_id.clone()),
    )
}

fn approved(tool: &str, grant: Option<String>, secret: Option<String>) -> Result<CommandResponse> {
    let outcome = match grant {
        Some(grant_id) => ApprovalOutcome::Approved(ApprovedApproval {
            tool_id: tool.into(),
            status: "approved".into(),
            grant_id,
            secrets_grant_id: secret,
        }),
        None => ApprovalOutcome::AlreadyApproved(AlreadyApproved {
            tool_id: tool.into(),
            status: "already_approved".into(),
        }),
    };
    Ok(CommandResponse {
        body: ResponseBody::Adapter(AdapterResponse::Approved(ApprovedResponse {
            status: "approved".into(),
            approvals: vec![outcome],
        })),
        completion: Completion::Complete,
    })
}
