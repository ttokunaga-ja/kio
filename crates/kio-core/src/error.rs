use serde_json::{Value, json};
use thiserror::Error;

use crate::ExitCode;

/// Resource exhaustion with enough information to report and recheck it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceLimit {
    pub resource: Resource,
    pub stage: String,
    pub limit: Option<u64>,
    pub observed: Option<u64>,
    pub resource_tier: Option<ResourceTier>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resource {
    Memory,
    Work,
    UnitText,
    RawBytes,
    MemberBytes,
    AggregateBytes,
    Storage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceTier {
    Standard,
    Large,
}

impl ResourceLimit {
    #[must_use]
    pub fn is_valid(&self) -> bool {
        !self.stage.is_empty()
            && self.stage.len() <= 256
            && !self.stage.chars().any(char::is_control)
    }

    #[must_use]
    pub fn matches_kind(kind: Option<&str>, limit: Option<&Self>) -> bool {
        match (kind, limit) {
            (Some("resource_limit"), Some(limit)) => limit.is_valid(),
            (Some("resource_limit"), None) => false,
            (_, None) => true,
            (_, Some(_)) => false,
        }
    }

    #[must_use]
    pub fn into_error(self, message: impl Into<String>) -> KioError {
        let mut context = serde_json::to_value(&self).expect("resource descriptor is serializable");
        context["retry_disposition"] = json!("explicit_after_resource_change");
        let mut error = KioError::new(
            "KIO-E-RESOURCE-LIMIT-001",
            message,
            context,
            ExitCode::PartialFailure,
        );
        error.resource_limit = Some(Box::new(self));
        error
    }
}

pub type Result<T> = std::result::Result<T, KioError>;

#[derive(Debug, Error)]
#[error("{error_code}: {message}")]
pub struct KioError {
    resource_limit: Option<Box<ResourceLimit>>,
    error_code: String,
    message: String,
    context: Value,
    exit_code: ExitCode,
}

impl KioError {
    #[must_use]
    pub fn new(
        error_code: impl Into<String>,
        message: impl Into<String>,
        context: Value,
        exit_code: ExitCode,
    ) -> Self {
        Self {
            resource_limit: None,
            error_code: error_code.into(),
            message: message.into(),
            context,
            exit_code,
        }
    }

    #[must_use]
    pub fn schema(message: impl Into<String>) -> Self {
        Self::new(
            "KIO-E-CONFIG-SCHEMA-001",
            message,
            json!({}),
            ExitCode::InvalidUsage,
        )
    }

    /// Path schema violation: a tree/pointer `path` that contains the path
    /// separator `/` (`docs/03-data-model.md` §3, `docs/06-cli-spec.md` §8).
    #[must_use]
    pub fn path(message: impl Into<String>, path: impl Into<String>) -> Self {
        Self::new(
            "KIO-E-STORE-PATH-001",
            message,
            json!({ "path": path.into() }),
            ExitCode::InvalidUsage,
        )
    }

    /// Duplicate `path` among the entries of a single tree
    /// (`docs/03-data-model.md` §8.1). Kept distinct from the `/`-in-path
    /// violation (`KIO-E-STORE-PATH-001`).
    #[must_use]
    pub fn duplicate_path(path: impl Into<String>) -> Self {
        Self::new(
            "KIO-E-STORE-DUP-001",
            "duplicate tree entry path",
            json!({ "path": path.into() }),
            ExitCode::InvalidUsage,
        )
    }

    #[must_use]
    pub fn not_found(hash: impl Into<String>) -> Self {
        Self::new(
            "KIO-E-STORE-NOT-FOUND-001",
            "object not found",
            json!({ "hash": hash.into() }),
            ExitCode::PermanentFailure,
        )
    }

    /// R15-4: the HEAD (or a named) commit is shallow — its tree object has been
    /// discarded (GC / manual deletion / corruption), so an operation that needs the
    /// full prior tree (snapshot / index / reindex, or a cursor replay) cannot
    /// proceed. Distinct from a raw `KIO-E-STORE-NOT-FOUND-001` so the caller gets a
    /// clear "shallow commit" signal plus recovery guidance instead of an opaque
    /// missing-object error (`docs/05-runtime.md` §2.2, `docs/06-cli-spec.md` §8).
    #[must_use]
    pub fn commit_shallow(message: impl Into<String>, commit_hash: impl Into<String>) -> Self {
        Self::new(
            "KIO-E-COMMIT-SHALLOW-001",
            message,
            json!({ "commit_hash": commit_hash.into() }),
            ExitCode::Failure,
        )
    }

    #[must_use]
    pub fn locked(path: impl Into<String>) -> Self {
        Self::new(
            "KIO-E-STORE-LOCKED-001",
            ".kio store is locked",
            json!({ "path": path.into() }),
            ExitCode::PartialFailure,
        )
    }

    #[must_use]
    pub fn io(message: impl Into<String>, path: impl Into<String>) -> Self {
        Self::new(
            "KIO-E-STORE-IO-001",
            message,
            json!({ "path": path.into() }),
            ExitCode::Failure,
        )
    }

    /// Invalid CLI usage / operand: a nonexistent `init` path, a directory that
    /// is not a `.kio` scope, a malformed hash argument, etc. Distinct from a
    /// JSON Schema violation (`KIO-E-CONFIG-SCHEMA-001`). Exit code stays 2.
    #[must_use]
    pub fn invalid_usage(message: impl Into<String>) -> Self {
        Self::new(
            "KIO-E-CONFIG-USAGE-001",
            message,
            json!({}),
            ExitCode::InvalidUsage,
        )
    }

    /// The canonical error for every non-current `kio_format_version` is
    /// `KIO-E-STORE-VERSION-001` (exit 8). The exact-version check runs before
    /// current-schema validation and never authorizes a compatibility reader.
    #[must_use]
    pub fn incompatible_format(found: impl Into<String>) -> Self {
        Self::new(
            "KIO-E-STORE-VERSION-001",
            "incompatible kio_format_version",
            json!({ "found": found.into() }),
            ExitCode::IncompatibleProfile,
        )
    }

    #[must_use]
    pub fn not_implemented(feature: impl Into<String>) -> Self {
        Self::new(
            "KIO-E-CONFIG-NOT-IMPLEMENTED-001",
            "not implemented",
            json!({ "feature": feature.into() }),
            ExitCode::Failure,
        )
    }

    #[must_use]
    pub fn error_code(&self) -> &str {
        &self.error_code
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    #[must_use]
    pub const fn exit_code(&self) -> ExitCode {
        self.exit_code
    }

    #[must_use]
    pub fn resource_limit(&self) -> Option<&ResourceLimit> {
        self.resource_limit.as_deref()
    }

    #[must_use]
    pub fn context(&self) -> &Value {
        &self.context
    }

    #[must_use]
    pub fn to_error_json(&self) -> Value {
        json!({
            "error_code": self.error_code,
            "message": self.message,
            "context": self.context,
        })
    }
}

pub trait IoResultExt<T> {
    fn kio_io(self, path: &std::path::Path) -> Result<T>;
}

impl<T> IoResultExt<T> for std::io::Result<T> {
    fn kio_io(self, path: &std::path::Path) -> Result<T> {
        self.map_err(|err| KioError::io(err.to_string(), path.display().to_string()))
    }
}

#[cfg(test)]
mod resource_limit_tests {
    use super::*;

    #[test]
    fn resource_limit_error_preserves_descriptor() {
        let descriptor = ResourceLimit {
            resource: Resource::Memory,
            stage: "local_result".to_owned(),
            limit: None,
            observed: Some(42),
            resource_tier: None,
        };
        let error = descriptor.into_error("insufficient memory");
        assert_eq!(error.resource_limit().unwrap().resource, Resource::Memory);
        assert_eq!(error.error_code(), "KIO-E-RESOURCE-LIMIT-001");
        assert_eq!(error.exit_code(), ExitCode::PartialFailure);
        assert_eq!(
            error.context(),
            &json!({
                "resource": "memory", "stage": "local_result", "limit": null,
                "observed": 42, "resource_tier": null,
                "retry_disposition": "explicit_after_resource_change"
            })
        );
    }
}
