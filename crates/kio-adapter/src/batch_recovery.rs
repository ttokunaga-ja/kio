//! Credential-bound batch recovery authority, distinct from an account identity.
//! Only the central ledger stores the opaque HMAC. Neither credentials nor
//! recovery authority belong in a tool profile, send grant, or scope document.

use std::fmt;
use std::fmt::Write as _;

use ring::hmac;

use crate::{AdapterError, Result};

const SCOPE_PREFIX: &str = "kio-batch-recovery:v1:";
const DOMAIN: &[u8] = b"kio.batch-recovery.scope.v1";

/// A separate device-private key; callers own its private storage lifecycle.
#[derive(Clone)]
pub struct BatchRecoveryContext {
    key: hmac::Key,
}

impl fmt::Debug for BatchRecoveryContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BatchRecoveryContext([REDACTED])")
    }
}

impl BatchRecoveryContext {
    #[must_use]
    pub fn from_key(key: [u8; 32]) -> Self {
        Self {
            key: hmac::Key::new(hmac::HMAC_SHA256, &key),
        }
    }

    pub(crate) fn capture(
        &self,
        provider: &str,
        origin: &str,
        qualifier: Option<&str>,
        credential: String,
    ) -> Result<PinnedBatchIdentity> {
        if credential.is_empty() {
            return Err(AdapterError::Auth("batch credential is empty".into()));
        }
        let origin = canonical_origin(origin)?;
        let mut mac = hmac::Context::with_key(&self.key);
        for field in [DOMAIN, provider.as_bytes(), origin.as_bytes()] {
            frame(&mut mac, field);
        }
        match qualifier {
            None => mac.update(&[0]),
            Some(value) => {
                mac.update(&[1]);
                frame(&mut mac, value.as_bytes());
            }
        }
        frame(&mut mac, credential.as_bytes());
        let signature = mac.sign();
        let mut scope = format!("{SCOPE_PREFIX}{provider}:");
        for byte in signature.as_ref() {
            write!(&mut scope, "{byte:02x}").expect("String writes do not fail");
        }
        Ok(PinnedBatchIdentity {
            credential,
            origin,
            scope,
        })
    }
}

fn frame(mac: &mut hmac::Context, value: &[u8]) {
    mac.update(&(value.len() as u64).to_be_bytes());
    mac.update(value);
}

/// Used only to decide whether a missing device key may be first-created.
/// Unknown versions are held too: they are never permission to rotate a key.
#[must_use]
pub fn is_bound_recovery_scope(scope: &str) -> bool {
    scope.starts_with("kio-batch-recovery:")
}

#[derive(Clone)]
pub(crate) struct PinnedBatchIdentity {
    credential: String,
    origin: String,
    scope: String,
}

impl fmt::Debug for PinnedBatchIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PinnedBatchIdentity([REDACTED])")
    }
}

impl PinnedBatchIdentity {
    pub(crate) fn credential(&self) -> &str {
        &self.credential
    }
    pub(crate) fn origin(&self) -> &str {
        &self.origin
    }
    pub(crate) fn scope(&self) -> &str {
        &self.scope
    }
}

/// Qualifiers are captured once with the credential. Invalid Unicode is an
/// invalid configuration, never an excuse to silently select another scope.
pub(crate) fn configured_qualifier(name: &str) -> Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) => Ok((!value.trim().is_empty()).then(|| value.trim().to_owned())),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(AdapterError::ConfigSchema(
            "batch workspace/project qualifier must be valid Unicode".into(),
        )),
    }
}

fn canonical_origin(input: &str) -> Result<String> {
    let invalid = || {
        AdapterError::ConfigSchema(
            "batch origin must be an HTTPS origin without userinfo, path, query, or fragment"
                .into(),
        )
    };
    let uri: ureq::http::Uri = input.parse().map_err(|_| invalid())?;
    let authority = uri.authority().ok_or_else(invalid)?;
    if uri.scheme_str() != Some("https")
        || authority.as_str().contains('@')
        || !matches!(uri.path(), "" | "/")
        || uri.query().is_some()
        || input.contains('#')
    {
        return Err(invalid());
    }
    let host = authority.host().to_ascii_lowercase();
    if host.is_empty() {
        return Err(invalid());
    }
    let port = authority.port_u16();
    if authority.port().is_some() && port.is_none() {
        return Err(invalid());
    }
    Ok(match port {
        Some(port) if port != 443 => format!("https://{host}:{port}"),
        _ => format!("https://{host}"),
    })
}

#[cfg(test)]
pub(crate) fn with_test_env_credential<T>(
    role: &str,
    variable: &str,
    operation: impl FnOnce() -> T,
) -> T {
    use crate::tool_lock::{
        AdapterRuntimeSettings, declared_adapter_for_role, with_runtime_settings,
    };
    use kio_core::store_dir::{Publication, StoreDirectory, restrict_new_private_directory};
    use std::path::Path;
    let temporary = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temporary.path()).unwrap();
    let parent = StoreDirectory::open(&root).unwrap();
    let directory = parent.create_directory(Path::new("credentials")).unwrap();
    restrict_new_private_directory(&directory).unwrap();
    let root = root.join("credentials");
    let store = StoreDirectory::from_retained(directory, root.clone()).unwrap();
    let source = format!("[{role}]\nauth = \"env:{variable}\"\n");
    store
        .write_atomic(
            Path::new("tools.toml"),
            source.as_bytes(),
            Publication::CreateOnly,
        )
        .unwrap();
    let value: toml::Value = toml::from_str(&source).unwrap();
    let declarations = std::collections::HashMap::from([(
        role.to_owned(),
        declared_adapter_for_role(&value, role).unwrap(),
    )]);
    with_runtime_settings(
        AdapterRuntimeSettings {
            declarations,
            tools_toml_path: Some(root.join("tools.toml")),
            ..Default::default()
        },
        operation,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_scope_is_stable_and_bound_to_every_authority_component() {
        let context = BatchRecoveryContext::from_key([7; 32]);
        let original = context
            .capture(
                "mistral",
                "https://api.example",
                Some("workspace"),
                "secret-a".into(),
            )
            .unwrap();
        let same = context
            .capture(
                "mistral",
                "https://API.EXAMPLE:443/",
                Some("workspace"),
                "secret-a".into(),
            )
            .unwrap();
        assert_eq!(original.scope(), same.scope());
        for (provider, origin, qualifier, credential) in [
            (
                "gemini",
                "https://api.example",
                Some("workspace"),
                "secret-a",
            ),
            (
                "mistral",
                "https://other.example",
                Some("workspace"),
                "secret-a",
            ),
            (
                "mistral",
                "https://api.example:444",
                Some("workspace"),
                "secret-a",
            ),
            ("mistral", "https://api.example", Some("other"), "secret-a"),
            ("mistral", "https://api.example", None, "secret-a"),
            (
                "mistral",
                "https://api.example",
                Some("workspace"),
                "secret-b",
            ),
            (
                "mistral",
                "https://api.example",
                Some("workspace"),
                "secret-a ",
            ),
        ] {
            assert_ne!(
                original.scope(),
                context
                    .capture(provider, origin, qualifier, credential.into())
                    .unwrap()
                    .scope()
            );
        }
        assert_ne!(
            original.scope(),
            BatchRecoveryContext::from_key([8; 32])
                .capture(
                    "mistral",
                    "https://api.example",
                    Some("workspace"),
                    "secret-a".into()
                )
                .unwrap()
                .scope()
        );
        assert_ne!(
            context
                .capture("mistral", "https://api.example", Some("ab"), "c".into())
                .unwrap()
                .scope(),
            context
                .capture("mistral", "https://api.example", Some("a"), "bc".into())
                .unwrap()
                .scope()
        );
        assert_ne!(
            context
                .capture("mistral", "https://api.example", None, "c".into())
                .unwrap()
                .scope(),
            context
                .capture("mistral", "https://api.example", Some(""), "c".into())
                .unwrap()
                .scope()
        );
        let debug = format!("{context:?} {original:?}");
        assert!(!debug.contains("secret-a"));
        assert!(!debug.contains(original.scope()));
        assert!(is_bound_recovery_scope(original.scope()));
        assert!(!is_bound_recovery_scope("mistral:default"));
    }

    #[test]
    fn recovery_origin_rejects_ambiguous_or_untrusted_routes() {
        for origin in [
            "http://api.example",
            "https://user@api.example",
            "https://api.example/path",
            "https://api.example?q=a",
            "https://api.example/#fragment",
        ] {
            assert!(canonical_origin(origin).is_err(), "{origin}");
        }
    }
}
