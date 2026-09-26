//! Live, fail-closed policy evaluation for managed scope hierarchies.
//!
//! This intentionally reads the local authority at every validated ancestor.
//! The old generated-parent-policy field is an optimization artifact, never an
//! authority input here.

use std::path::{Component, Path, PathBuf};

use kio_core::cas::hash_bytes;
use kio_core::management::{ManagementBinding, validate_live_chain};
use kio_core::store_dir::StoreDirectory;
use sha2::{Digest, Sha256};

use crate::scan::{
    IgnoreRule, SecretTier, classify_secret, parse_kioignore_text, parse_local_config_ignore_text,
    rules_explicitly_unignore_path, rules_ignore_path,
};
use crate::{PipelineError, Result};

/// The policy grammar and digest framing. Bump this when policy semantics
/// change, so stale projections and approvals cannot cross the boundary.
pub const CURRENT_POLICY_TEMPLATE_VERSION: &str = "kio-current-policy-v1";
pub const MAX_CURRENT_POLICY_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone)]
struct LocalPolicy {
    root: PathBuf,
    rules: Vec<IgnoreRule>,
    config_hash: String,
    ignore_hash: String,
}

/// A fresh snapshot of the current effective management and ignore authority.
/// `case_insensitive` is supplied by the persistent filesystem-capability
/// binding; this evaluator never probes or writes a case-test file.
#[derive(Debug, Clone)]
pub struct CurrentPolicyEvaluator {
    binding: ManagementBinding,
    case_insensitive: bool,
    scopes: Vec<LocalPolicy>, // target first, then direct ancestors
    digest: String,
}

impl CurrentPolicyEvaluator {
    pub fn load(binding: &ManagementBinding, case_insensitive: bool) -> Result<Self> {
        let chain = validate_live_chain(binding).map_err(management_error)?;
        let persisted_case_mode = chain
            .scopes
            .first()
            .expect("validated management chain is nonempty")
            .record
            .case_insensitive;
        if case_insensitive != persisted_case_mode {
            return Err(PipelineError::contract(
                "KIO-E-POLICY-CASE-MODE-001",
                "requested case mode does not match the persisted management capability",
            ));
        }
        let mut scopes = Vec::with_capacity(chain.scopes.len());
        for scope in &chain.scopes {
            let root = scope.binding.canonical_root().to_path_buf();
            let kio = retained_directory(
                scope.binding.kio_handle(),
                root.join(".kio"),
                "retained .kio handle",
            )?;
            let root_directory = retained_directory(
                scope.binding.root_handle(),
                root.clone(),
                "retained scope root handle",
            )?;
            let (config_bytes, ignore_bytes) = (
                kio.read_optional(Path::new("config.toml"), MAX_CURRENT_POLICY_FILE_BYTES)
                    .map_err(policy_file_error)?
                    .ok_or_else(|| {
                        PipelineError::contract(
                            "KIO-E-POLICY-CONFIG-MISSING-001",
                            "managed scope has no config.toml policy authority",
                        )
                    })?,
                root_directory
                    .read_optional(Path::new(".kioignore"), MAX_CURRENT_POLICY_FILE_BYTES)
                    .map_err(policy_file_error)?,
            );
            let mut rules = parse_local_config_ignore_text(utf8(&config_bytes, "config.toml")?)?;
            if let Some(bytes) = ignore_bytes.as_deref() {
                rules.extend(parse_kioignore_text(utf8(bytes, ".kioignore")?)?);
            }
            scopes.push(LocalPolicy {
                root,
                rules,
                config_hash: hash_bytes(&config_bytes),
                ignore_hash: optional_hash(ignore_bytes.as_deref()),
            });
        }
        let after = validate_live_chain(binding).map_err(management_error)?;
        if after.digest_input != chain.digest_input {
            return Err(PipelineError::contract(
                "KIO-E-POLICY-CHAIN-CHANGED-001",
                "management authority changed while policy inputs were read",
            ));
        }
        let digest = policy_digest(&chain.digest_input, case_insensitive, &scopes);
        Ok(Self {
            binding: binding.clone(),
            case_insensitive,
            scopes,
            digest,
        })
    }

    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }

    #[must_use]
    pub fn case_insensitive(&self) -> bool {
        self.case_insensitive
    }

    /// Reloads the live chain and returns an error when any management or local
    /// policy input differs from this snapshot. Callers must not treat a cache
    /// or watcher event as a substitute for this send/search-time check.
    pub fn revalidate(&self) -> Result<()> {
        let current = Self::load(&self.binding, self.case_insensitive)?;
        if current.digest != self.digest {
            return Err(PipelineError::contract(
                "KIO-E-POLICY-STALE-001",
                "management or local ignore authority changed since policy evaluation",
            ));
        }
        Ok(())
    }

    /// Whether a relative user path is currently eligible. This is a pure
    /// policy query: deleted history entries are evaluated by name and do not
    /// require that a filesystem object currently exists.
    pub fn allows_path(&self, name: &str) -> Result<bool> {
        if is_control_path(name) {
            return Ok(false);
        }
        let relative = checked_user_relative_path(name)?;
        if !self.allows_scope()? {
            return Ok(false);
        }
        for scope in &self.scopes {
            let path = target_relative_path(self.binding.canonical_root(), scope, &relative)?;
            if rules_ignore_path(&path, false, &scope.rules, self.case_insensitive)? {
                return Ok(false);
            }
        }
        if classify_secret(&relative) == Some(SecretTier::TierA) {
            let target = self
                .scopes
                .first()
                .expect("validated management chain is nonempty");
            let local_path = relative.as_str();
            let locally_unignored =
                !rules_ignore_path(local_path, false, &target.rules, self.case_insensitive)?
                    && rules_explicitly_unignore_path(
                        local_path,
                        false,
                        &target.rules,
                        self.case_insensitive,
                    )?;
            return Ok(locally_unignored);
        }
        Ok(true)
    }

    /// Whether the managed target directory remains eligible through every
    /// ancestor. This is deliberately separate from path eligibility.
    pub fn allows_scope(&self) -> Result<bool> {
        for scope in &self.scopes {
            let path = target_relative_path(self.binding.canonical_root(), scope, "")?;
            if !path.is_empty()
                && rules_ignore_path(&path, true, &scope.rules, self.case_insensitive)?
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Check a prospective descendant before creating its management store.
    /// Each ancestor is evaluated separately so a local negation cannot lift
    /// a denial inherited from a different authority.
    pub fn allows_directory(&self, name: &str) -> Result<bool> {
        if is_control_path(name) || !self.allows_scope()? {
            return Ok(false);
        }
        let relative = checked_user_relative_path(name)?;
        for scope in &self.scopes {
            let path = target_relative_path(self.binding.canonical_root(), scope, &relative)?;
            if rules_ignore_path(&path, true, &scope.rules, self.case_insensitive)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Project every live ancestor rule onto a direct child of this managed
    /// scope for retained child discovery. The result remains in memory and is
    /// never serialized into child configuration.
    pub(crate) fn projected_rules_for_child(&self, child: &str) -> Result<Vec<IgnoreRule>> {
        if !child.is_empty() {
            checked_user_relative_path(child)?;
        }
        let mut projected = Vec::new();
        for scope in &self.scopes {
            let prefix = target_relative_path(self.binding.canonical_root(), scope, child)?;
            for rule in &scope.rules {
                let mut rule = rule.clone();
                rule.scope_prefix = Some(prefix.clone());
                projected.push(rule);
            }
        }
        Ok(projected)
    }
}

fn checked_user_relative_path(name: &str) -> Result<String> {
    if name.is_empty() || name.contains('\\') || Path::new(name).is_absolute() {
        return Err(PipelineError::path(name));
    }
    let path = Path::new(name);
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(PipelineError::path(name));
    }
    Ok(name.replace(std::path::MAIN_SEPARATOR, "/"))
}

fn is_control_path(name: &str) -> bool {
    Path::new(name).components().any(|component| {
        matches!(component, Component::Normal(value) if value == ".kio" || value == ".kioignore")
    })
}

fn target_relative_path(target: &Path, scope: &LocalPolicy, suffix: &str) -> Result<String> {
    let relative = target.strip_prefix(&scope.root).map_err(|_| {
        PipelineError::contract(
            "KIO-E-POLICY-CHAIN-001",
            "validated management ancestor does not contain target scope",
        )
    })?;
    let mut value = relative.to_string_lossy().replace('\\', "/");
    if !suffix.is_empty() {
        if !value.is_empty() {
            value.push('/');
        }
        value.push_str(suffix);
    }
    Ok(value)
}

fn retained_directory(
    handle: &std::fs::File,
    logical: PathBuf,
    label: &str,
) -> Result<StoreDirectory> {
    let handle = handle.try_clone().map_err(|error| PipelineError::Io {
        path: logical.display().to_string(),
        message: format!("cannot clone {label}: {error}"),
    })?;
    StoreDirectory::from_retained(handle, logical).map_err(policy_file_error)
}

fn policy_file_error(error: kio_core::KioError) -> PipelineError {
    PipelineError::contract("KIO-E-POLICY-FILE-IDENTITY-001", error.to_string())
}

fn utf8<'a>(bytes: &'a [u8], name: &str) -> Result<&'a str> {
    std::str::from_utf8(bytes)
        .map_err(|error| PipelineError::Schema(format!("{name} is not UTF-8: {error}")))
}

fn optional_hash(bytes: Option<&[u8]>) -> String {
    bytes.map_or_else(|| "absent".to_owned(), hash_bytes)
}

fn policy_digest(management: &[u8], case_insensitive: bool, scopes: &[LocalPolicy]) -> String {
    let mut hash = Sha256::new();
    for field in [
        CURRENT_POLICY_TEMPLATE_VERSION.as_bytes(),
        if case_insensitive {
            b"case-insensitive"
        } else {
            b"case-sensitive"
        },
        management,
    ] {
        hash.update((field.len() as u64).to_be_bytes());
        hash.update(field);
    }
    for scope in scopes {
        for field in [
            scope.root.to_string_lossy().as_bytes(),
            scope.config_hash.as_bytes(),
            scope.ignore_hash.as_bytes(),
        ] {
            hash.update((field.len() as u64).to_be_bytes());
            hash.update(field);
        }
    }
    hash_bytes(&hash.finalize())
}

fn management_error(error: kio_core::KioError) -> PipelineError {
    PipelineError::contract("KIO-E-POLICY-MANAGEMENT-001", error.to_string())
}
