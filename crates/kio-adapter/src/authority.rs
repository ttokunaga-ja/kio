//! Non-secret runtime execution identities for external-send grants.
//!
//! This module is deliberately a description of the production transport that
//! the built-in clients will use.  It neither reads a credential value nor
//! sends a probe.  Callers persist this identity with a grant and re-create it
//! immediately before send, so a changed declaration, endpoint, or local CA
//! cannot silently inherit an earlier approval.

use serde::{Deserialize, Serialize};

use crate::tool_lock::{self, DeclaredAdapter};
use crate::{AdapterError, Result};

const MISTRAL_OCR_AND_BATCH_DESTINATIONS: &str = "https://api.mistral.ai/v1/batch/jobs|https://api.mistral.ai/v1/files|https://api.mistral.ai/v1/ocr";
const GEMINI_API_ORIGIN: &str = "https://generativelanguage.googleapis.com";
const ONLINE_TRANSPORT_BINDING: &str = "web-pki:https-only:no-proxy:no-redirect";

/// App-supplied output-profile identity to bind to the current runtime.
///
/// `tool_profile_hash` is intentionally supplied by the app: it identifies
/// reusable output, while this module identifies the execution authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeExecutionIdentityRequest<'a> {
    pub role: &'a str,
    pub tool_id: &'a str,
    pub tool_profile_hash: &'a str,
}

/// Stable, non-secret identity used by device grant approval and send-time
/// matching.  Credential references never contain credential material; local
/// trust is a freshly read CA-bundle digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeExecutionIdentity {
    pub role: String,
    pub tool_id: String,
    pub tool_profile_hash: String,
    pub execution_mode: String,
    /// Pipe-delimited, lexically sorted production request destinations.
    /// Each entry is a route that can receive user content; polling and
    /// provider inventory routes are not egress destinations.
    pub canonical_destination: String,
    /// `env:<variable-name>`, an opaque `plain:tools.toml:<role>` slot, or
    /// `unconfigured` when the current client has no credential source.
    pub credential_binding: String,
    pub trust_binding: String,
}

/// Build an identity for the runtime declaration installed by the composition
/// root. Built-in online targets retain their fixed destination identity when a
/// declaration is absent, but expose their missing credential as
/// `unconfigured`; unsupported and declared-invalid targets fail closed.
pub fn runtime_execution_identity(
    request: RuntimeExecutionIdentityRequest<'_>,
) -> Result<RuntimeExecutionIdentity> {
    if request.tool_profile_hash.is_empty() {
        return Err(schema("tool_profile_hash must not be empty"));
    }
    let declared = tool_lock::registered_declared_adapter(request.role);
    if let Some(declared) = declared.as_ref() {
        tool_lock::validate_declared_runtime_target(request.role, declared)?;
    }

    match request.role {
        "markdown" => markdown_identity(request, declared.as_ref()),
        "embedding" => embedding_identity(request, declared.as_ref()),
        // `tool_lock` currently reserves this role, so it cannot supply a
        // registered, validated local reranker.  There is also no online
        // rerank implementation to describe.  Do not mint an approval for
        // either imagined target.
        "rerank" => Err(schema("rerank has no registered supported runtime")),
        role => Err(schema(format!("unsupported adapter role `{role}`"))),
    }
}

fn markdown_identity(
    request: RuntimeExecutionIdentityRequest<'_>,
    declared: Option<&DeclaredAdapter>,
) -> Result<RuntimeExecutionIdentity> {
    let tool_id = effective_tool_id(declared, "mistral_ocr_markdownize", "paddleocr_vl_local");
    require_requested_tool_id(request.tool_id, tool_id)?;
    match tool_id {
        "mistral_ocr_markdownize" => online_identity(
            request,
            tool_id,
            declared,
            MISTRAL_OCR_AND_BATCH_DESTINATIONS,
        ),
        "paddleocr_vl_local" => local_identity(request, tool_id, declared),
        _ => Err(schema("declared markdown target is unsupported")),
    }
}

fn embedding_identity(
    request: RuntimeExecutionIdentityRequest<'_>,
    declared: Option<&DeclaredAdapter>,
) -> Result<RuntimeExecutionIdentity> {
    let tool_id = effective_tool_id(declared, "gemini_embedding_2", "qwen3_vl_embedding_local");
    require_requested_tool_id(request.tool_id, tool_id)?;
    match tool_id {
        "gemini_embedding_2" => {
            let model = declared
                .and_then(|declared| declared.model.as_deref())
                .unwrap_or("gemini-embedding-2");
            let destination = format!(
                "{GEMINI_API_ORIGIN}/v1beta/models/{model}:asyncBatchEmbedContent|{GEMINI_API_ORIGIN}/v1beta/models/{model}:batchEmbedContents"
            );
            online_identity(request, tool_id, declared, &destination)
        }
        "qwen3_vl_embedding_local" => local_identity(request, tool_id, declared),
        _ => Err(schema("declared embedding target is unsupported")),
    }
}

fn effective_tool_id<'a>(
    declared: Option<&'a DeclaredAdapter>,
    online: &'static str,
    offline: &'static str,
) -> &'a str {
    declared
        .and_then(|declared| declared.tool_id.as_deref())
        .unwrap_or_else(|| {
            if declared.and_then(|declared| declared.kind.as_deref()) == Some("offline_api") {
                offline
            } else {
                online
            }
        })
}

fn online_identity(
    request: RuntimeExecutionIdentityRequest<'_>,
    tool_id: &str,
    declared: Option<&DeclaredAdapter>,
    canonical_destination: &str,
) -> Result<RuntimeExecutionIdentity> {
    let credential_binding = credential_binding(
        declared.and_then(|declared| declared.auth.as_deref()),
        request.role,
    )?;
    Ok(RuntimeExecutionIdentity {
        role: request.role.to_owned(),
        tool_id: tool_id.to_owned(),
        tool_profile_hash: request.tool_profile_hash.to_owned(),
        execution_mode: "online_api".to_owned(),
        canonical_destination: canonical_destination.to_owned(),
        credential_binding,
        trust_binding: ONLINE_TRANSPORT_BINDING.to_owned(),
    })
}

fn local_identity(
    request: RuntimeExecutionIdentityRequest<'_>,
    tool_id: &str,
    declared: Option<&DeclaredAdapter>,
) -> Result<RuntimeExecutionIdentity> {
    let url = declared
        .ok_or_else(|| schema("offline_api adapter has no declaration"))?
        .url
        .as_deref()
        .ok_or_else(|| schema("offline_api adapter has no declared url"))?;
    let endpoint = tool_lock::authenticated_local_endpoint(request.role, url)?;
    Ok(RuntimeExecutionIdentity {
        role: request.role.to_owned(),
        tool_id: tool_id.to_owned(),
        tool_profile_hash: request.tool_profile_hash.to_owned(),
        execution_mode: "offline_api".to_owned(),
        canonical_destination: endpoint.destination_binding().to_owned(),
        credential_binding: "none".to_owned(),
        // Endpoint construction re-validates the certificate bytes against
        // the captured digest. The grant additionally binds the managed
        // lifecycle generation, preventing an A→B→A CA sequence from
        // reviving a grant minted for the original A generation.
        trust_binding: tool_lock::registered_local_peer_trust_binding()?,
    })
}

fn credential_binding(auth: Option<&str>, role: &str) -> Result<String> {
    let Some(auth) = auth else {
        return Ok("unconfigured".to_owned());
    };
    if let Some(name) = auth.strip_prefix("env:") {
        if name.is_empty() {
            return Err(schema("credential environment variable name is empty"));
        }
        return Ok(format!("env:{name}"));
    }
    if auth.strip_prefix("plain:").is_some() {
        // `DeclaredAdapter.auth` contains the literal form.  It must never be
        // copied or hashed here; this role slot is the only persisted binding.
        return Ok(format!("plain:tools.toml:{role}"));
    }
    Err(schema(
        "adapter auth binding must start with env: or plain:",
    ))
}

fn require_requested_tool_id(requested: &str, effective: &str) -> Result<()> {
    if requested == effective {
        Ok(())
    } else {
        Err(schema(format!(
            "app supplied tool_id `{requested}` does not match effective runtime `{effective}`"
        )))
    }
}

fn schema(message: impl Into<String>) -> AdapterError {
    AdapterError::ConfigSchema(message.into())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::tool_lock::{AdapterRuntimeSettings, with_runtime_settings};

    fn declared(tool_id: &str, kind: &str, auth: &str) -> DeclaredAdapter {
        DeclaredAdapter {
            tool_id: Some(tool_id.to_owned()),
            kind: Some(kind.to_owned()),
            auth: Some(auth.to_owned()),
            model: Some(
                if tool_id == "gemini_embedding_2" {
                    "gemini-embedding-2"
                } else {
                    "mistral-ocr-2505"
                }
                .to_owned(),
            ),
            ..DeclaredAdapter::default()
        }
    }

    fn with_declaration<T>(
        role: &str,
        declaration: DeclaredAdapter,
        operation: impl FnOnce() -> T,
    ) -> T {
        let mut declarations = HashMap::new();
        declarations.insert(role.to_owned(), declaration);
        with_runtime_settings(
            AdapterRuntimeSettings {
                declarations,
                ..AdapterRuntimeSettings::default()
            },
            operation,
        )
    }

    #[test]
    fn undeclared_builtin_uses_the_catalog_default_without_reading_its_secret() {
        let identity = runtime_execution_identity(RuntimeExecutionIdentityRequest {
            role: "embedding",
            tool_id: "gemini_embedding_2",
            tool_profile_hash: "sha256:builtin-profile",
        })
        .unwrap();
        assert_eq!(identity.credential_binding, "unconfigured");
        assert_eq!(identity.tool_profile_hash, "sha256:builtin-profile");
        assert!(
            identity
                .canonical_destination
                .contains(":batchEmbedContents")
        );
    }

    #[test]
    fn mistral_identity_covers_sync_and_batch_send_routes_without_reading_secret() {
        with_declaration(
            "markdown",
            declared(
                "mistral_ocr_markdownize",
                "online_api",
                "env:MISTRAL_API_KEY",
            ),
            || {
                let identity = runtime_execution_identity(RuntimeExecutionIdentityRequest {
                    role: "markdown",
                    tool_id: "mistral_ocr_markdownize",
                    tool_profile_hash: "sha256:profile",
                })
                .unwrap();
                assert_eq!(identity.execution_mode, "online_api");
                assert_eq!(identity.credential_binding, "env:MISTRAL_API_KEY");
                assert_eq!(
                    identity.canonical_destination,
                    MISTRAL_OCR_AND_BATCH_DESTINATIONS
                );
            },
        );
    }

    #[test]
    fn gemini_identity_covers_sync_and_async_batch_send_routes() {
        with_declaration(
            "embedding",
            declared(
                "gemini_embedding_2",
                "online_api",
                "plain:NEVER_PERSIST_THIS",
            ),
            || {
                let identity = runtime_execution_identity(RuntimeExecutionIdentityRequest {
                    role: "embedding",
                    tool_id: "gemini_embedding_2",
                    tool_profile_hash: "sha256:profile",
                })
                .unwrap();
                assert_eq!(identity.credential_binding, "plain:tools.toml:embedding");
                assert!(!format!("{identity:?}").contains("NEVER_PERSIST_THIS"));
                assert!(
                    identity
                        .canonical_destination
                        .contains(":batchEmbedContents")
                );
                assert!(
                    identity
                        .canonical_destination
                        .contains(":asyncBatchEmbedContent")
                );
            },
        );
    }

    #[test]
    fn supplied_tool_must_match_effective_runtime() {
        with_declaration(
            "markdown",
            declared(
                "mistral_ocr_markdownize",
                "online_api",
                "env:MISTRAL_API_KEY",
            ),
            || {
                assert!(
                    runtime_execution_identity(RuntimeExecutionIdentityRequest {
                        role: "markdown",
                        tool_id: "paddleocr_vl_local",
                        tool_profile_hash: "sha256:profile",
                    })
                    .is_err()
                );
            },
        );
    }

    #[test]
    fn declared_online_target_without_auth_exposes_an_unconfigured_binding() {
        let mut declaration = declared("gemini_embedding_2", "online_api", "env:unused");
        declaration.auth = None;
        with_declaration("embedding", declaration, || {
            let identity = runtime_execution_identity(RuntimeExecutionIdentityRequest {
                role: "embedding",
                tool_id: "gemini_embedding_2",
                tool_profile_hash: "sha256:profile",
            })
            .unwrap();
            assert_eq!(identity.credential_binding, "unconfigured");
        });
    }

    #[test]
    fn declared_credential_reference_changes_the_identity_from_unconfigured() {
        let unconfigured = runtime_execution_identity(RuntimeExecutionIdentityRequest {
            role: "markdown",
            tool_id: "mistral_ocr_markdownize",
            tool_profile_hash: "sha256:profile",
        })
        .unwrap();
        with_declaration(
            "markdown",
            declared(
                "mistral_ocr_markdownize",
                "online_api",
                "env:MISTRAL_API_KEY",
            ),
            || {
                let configured = runtime_execution_identity(RuntimeExecutionIdentityRequest {
                    role: "markdown",
                    tool_id: "mistral_ocr_markdownize",
                    tool_profile_hash: "sha256:profile",
                })
                .unwrap();
                assert_ne!(configured, unconfigured);
                assert_eq!(configured.credential_binding, "env:MISTRAL_API_KEY");
            },
        );
    }

    #[test]
    fn empty_profile_identity_is_rejected_before_authority_is_created() {
        assert!(
            runtime_execution_identity(RuntimeExecutionIdentityRequest {
                role: "markdown",
                tool_id: "mistral_ocr_markdownize",
                tool_profile_hash: "",
            })
            .is_err()
        );
    }

    #[test]
    fn profile_change_produces_a_distinct_send_authority_identity() {
        let first = runtime_execution_identity(RuntimeExecutionIdentityRequest {
            role: "markdown",
            tool_id: "mistral_ocr_markdownize",
            tool_profile_hash: "sha256:first",
        })
        .unwrap();
        let second = runtime_execution_identity(RuntimeExecutionIdentityRequest {
            role: "markdown",
            tool_id: "mistral_ocr_markdownize",
            tool_profile_hash: "sha256:second",
        })
        .unwrap();
        assert_ne!(first, second);
    }

    #[cfg(unix)]
    #[test]
    fn local_identity_binds_normalized_endpoint_and_current_ca_digest() {
        let directory = tempfile::Builder::new()
            .prefix("kio-authority-local-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let rcgen::CertifiedKey { cert, .. } =
            rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
        let ca_path = directory.path().join("peer-ca.pem");
        fs::write(&ca_path, cert.pem()).unwrap();
        fs::set_permissions(&ca_path, fs::Permissions::from_mode(0o600)).unwrap();
        let trust_digest = crate::local_peer::capture_private_local_trust(&ca_path).unwrap();

        let declaration = DeclaredAdapter {
            tool_id: Some("qwen3_vl_embedding_local".to_owned()),
            kind: Some("offline_api".to_owned()),
            url: Some("https://127.0.0.1:8443/".to_owned()),
            model: Some("Qwen/Qwen3-VL-Embedding-2B".to_owned()),
            ..DeclaredAdapter::default()
        };
        let mut declarations = HashMap::new();
        declarations.insert("embedding".to_owned(), declaration);
        with_runtime_settings(
            AdapterRuntimeSettings {
                declarations,
                local_peer_ca_path: Some(ca_path),
                local_peer_trust_digest: Some(trust_digest),
                local_peer_trust_binding: Some("local-ca:v1:g1:sha256:test".to_owned()),
                ..AdapterRuntimeSettings::default()
            },
            || {
                let identity = runtime_execution_identity(RuntimeExecutionIdentityRequest {
                    role: "embedding",
                    tool_id: "qwen3_vl_embedding_local",
                    tool_profile_hash: "sha256:local-profile",
                })
                .unwrap();
                assert_eq!(identity.canonical_destination, "https://127.0.0.1:8443");
                assert_eq!(identity.trust_binding, "local-ca:v1:g1:sha256:test");
                assert_eq!(identity.credential_binding, "none");
            },
        );
    }
}
