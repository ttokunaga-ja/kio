//! Scan preview contracts.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
#[cfg(not(windows))]
use std::fs::Metadata;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use cap_primitives::{ambient_authority, fs as cap_fs};
use kio_core::management::{DirectoryIdentity, ManagementBinding, read_record};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::policy::CurrentPolicyEvaluator;
use crate::prepare::{hash_bytes, hash_reader};
use crate::{IoResultExt, Result};

const MAX_GLOB_STATES: usize = 100_000;
/// Child-scope discovery is deliberately bounded: it is an index convenience,
/// never a general filesystem crawler.
const MAX_CHILD_SCOPE_DEPTH: usize = 32;
const MAX_CHILD_SCOPE_DIRECTORIES: usize = 512;
/// One resumable discovery slice never returns or retains more than this many
/// candidate rows. The caller must durable-store the continuation before a
/// later slice; this is intentionally separate from the legacy one-shot cap.
pub const CHILD_SCOPE_DISCOVERY_PAGE_SIZE: usize = 128;
const CHILD_SCOPE_DISCOVERY_SELECTION_SIZE: usize = CHILD_SCOPE_DISCOVERY_PAGE_SIZE + 1;
const CHILD_SCOPE_DISCOVERY_TOKEN_VERSION: u8 = 1;
const MAX_CHILD_SCOPE_DISCOVERY_TOKEN_BYTES: usize = 1024 * 1024;
/// A Unix pathname is bounded by the operating system at open time. This cap
/// bounds continuation serialization without imposing Unix's short path
/// limit on Windows, whose normal long-path ceiling is 32,767 UTF-16 units.
#[cfg(not(windows))]
const MAX_CHILD_SCOPE_DISCOVERY_RELATIVE_BYTES: usize = 128 * 1024;
#[cfg(windows)]
const MAX_CHILD_SCOPE_DISCOVERY_RELATIVE_UTF16: usize = 32_767;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildScopeDiscovery {
    /// Relative to the parent scope, using `/` separators.
    pub path: String,
    /// `planned`, `skipped_vcs`, `skipped_ignored`, `skipped_unreadable`, or a
    /// bound status. The CLI changes `planned` to `indexed` after success.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

fn child_status(path: String, status: &str) -> ChildScopeDiscovery {
    ChildScopeDiscovery {
        path,
        status: status.to_owned(),
        reason: None,
        error_code: None,
        message: None,
    }
}

pub struct ChildScopePlan {
    pub candidates: Vec<ChildScopeDiscovery>,
    root_handle: File,
    planned_handles: BTreeMap<String, File>,
    canonical_roots: BTreeMap<String, PathBuf>,
    index_vcs_repos: bool,
    // Rules effective at the parent root.  A bounded, prefix-qualified copy is
    // passed to each bound child before it scans, so a later child-only index
    // cannot accidentally forget an ancestor's privacy policy.
    effective_ignore_rules: Vec<IgnoreRule>,
}

/// A discovery-time child rebound through its retained parent directory.
/// `root` is the only I/O authority for the child; `canonical_root` is for
/// display and repository identity binding, not a reopening instruction.
pub struct BoundPlannedChild {
    pub canonical_root: PathBuf,
    pub root: File,
    pub inherited_rules: Vec<IgnoreRule>,
}

/// Durable, path-only DFS state for one child-scope discovery continuation.
/// Handles are deliberately absent: a later process must establish a fresh
/// [`ManagementBinding`] and re-open every frame no-follow under its retained
/// root before it may continue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildScopeDiscoveryContinuation {
    pub version: u8,
    pub scope_id: String,
    pub root_identity: DirectoryIdentity,
    pub case_insensitive: bool,
    pub policy_digest: String,
    pub index_vcs_repos: bool,
    pub frames: Vec<ChildScopeDiscoveryFrame>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildScopeDiscoveryFrame {
    /// Relative to the managed root, empty only for the root frame.
    pub relative_dir: String,
    pub expected_identity: DirectoryIdentity,
    /// Last name whose result has been committed to this traversal. Names are
    /// UTF-8 and compared bytewise; non-UTF-8 names remain unsupported exactly
    /// as in the legacy discovery API.
    pub after_entry: Option<String>,
    /// The next bounded, sorted page after `after_entry`. Holding at most 129
    /// names avoids rescanning once per child while keeping token memory finite.
    pub pending_entries: Vec<String>,
}

/// A retained-handle page. Its `plan` is valid only for this process; callers
/// must bind/enroll planned children before persisting `continuation`.
pub struct ChildScopeDiscoverySlice {
    pub plan: ChildScopePlan,
    pub continuation: Option<ChildScopeDiscoveryContinuation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanCandidate {
    pub input_path: String,
    pub media_type: String,
    pub size_bytes: u64,
    pub raw_hash: Option<String>,
    pub ignored: bool,
    pub quarantine_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScanPreview {
    pub scope_id: String,
    pub candidates: Vec<ScanCandidate>,
    pub estimated_cost: Option<CostPreview>,
    pub approval_required: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostPreview {
    /// `estimated_markdownize_usd + estimated_embedding_usd` (kept as a
    /// combined figure for existing display call sites).
    pub estimated_usd: f64,
    pub budget_cap_usd: Option<f64>,
    pub budget_warning: Option<String>,
    /// QA19 (step4b-contract-tests-p3a.md §F, 10 §1 L48-53): `tools.toml`'s
    /// `[markdown.*.pricing]` unit price × an estimated page count derived
    /// from non-text-native candidate bytes. `0.0` when no `pricing` is
    /// declared for the markdownize role — an honest "unknown" rather than a
    /// fabricated figure.
    #[serde(default)]
    pub estimated_markdownize_usd: f64,
    /// QA19: `tools.toml`'s `[embedding.*.pricing]` unit price × an estimated
    /// token count derived from all included candidate bytes. `0.0` when no
    /// `pricing` is declared for the embedding role.
    #[serde(default)]
    pub estimated_embedding_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanPreviewRequest {
    pub scope_path: String,
    pub include_raw_hashes: bool,
    pub require_network_approval: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretTier {
    TierA,
    TierB,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IgnoreRule {
    pub pattern: String,
    pub negated: bool,
    /// Prefix of the scope in which this rule was originally authored.  Local
    /// rules have no prefix; generated parent rules are evaluated against the
    /// ancestor-relative path.  This preserves glob/negation semantics across
    /// arbitrarily nested child scopes without trying to rewrite globs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_prefix: Option<String>,
}

const MAX_GENERATED_PARENT_IGNORE_RULES: usize = 1_024;
const MAX_GENERATED_PARENT_IGNORE_PATTERN_BYTES: usize = 4_096;
const MAX_GENERATED_PARENT_IGNORE_PREFIX_BYTES: usize = 4_096;
/// Bound child reads config, scope metadata, and `.kioignore` through retained
/// descriptors. Keep those parser inputs finite before allocating them.
const MAX_BOUND_SCAN_METADATA_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GeneratedParentPolicy {
    rules: Vec<IgnoreRule>,
}

/// Return the effective parent policy as it applies beneath `relative`.
/// This payload is deliberately bounded before it crosses the internal CLI
/// process boundary.  The receiving child validates it again before persisting.
pub fn generated_parent_policy_for_child(
    plan: &ChildScopePlan,
    relative: &str,
) -> Result<Vec<IgnoreRule>> {
    validate_scope_prefix(relative)?;
    if plan.effective_ignore_rules.len() > MAX_GENERATED_PARENT_IGNORE_RULES {
        return Err(crate::PipelineError::Schema(
            "generated parent ignore policy exceeds rule cap".to_owned(),
        ));
    }
    plan.effective_ignore_rules
        .iter()
        .cloned()
        .map(|mut rule| {
            validate_ignore_rule(&rule)?;
            rule.scope_prefix = Some(join_scope_prefix(rule.scope_prefix.as_deref(), relative));
            validate_ignore_rule(&rule)?;
            Ok(rule)
        })
        .collect()
}

pub fn build_scan_preview(request: ScanPreviewRequest) -> Result<ScanPreview> {
    let scope_path = PathBuf::from(&request.scope_path);
    let inherited = load_generated_parent_ignore(&scope_path)?;
    build_scan_preview_with_inherited_rules(request, &inherited)
}

/// Build a mutation-free direct-file preview with explicitly supplied ancestor
/// rules.  This lets a parent disclose the same child totals it would persist
/// during a real bound-child index, without trusting a stale child config.
pub fn build_scan_preview_with_inherited_rules(
    request: ScanPreviewRequest,
    inherited_rules: &[IgnoreRule],
) -> Result<ScanPreview> {
    let scope_path = PathBuf::from(&request.scope_path);
    // R10-3: probe the scope volume's case sensitivity ONCE per scan (git
    // `core.ignorecase` equivalent) so ignore matching can fold case on a
    // case-insensitive FS (APFS default) without folding on a case-sensitive one.
    let case_insensitive = probe_case_insensitive(&scope_path);
    let mut ignore_rules = inherited_rules.to_vec();
    ignore_rules.extend(load_local_config_ignore(&scope_path)?);
    ignore_rules.extend(load_kioignore(&scope_path)?);
    let mut candidates = Vec::new();
    collect_direct_candidates(
        &scope_path,
        &ignore_rules,
        request.include_raw_hashes,
        case_insensitive,
        &mut candidates,
    )?;
    apply_inherited_denials(&mut candidates, inherited_rules, case_insensitive)?;
    candidates.sort_by(|a, b| a.input_path.cmp(&b.input_path));
    let markdownize_pricing = kio_adapter::tool_lock::registered_declared_pricing("markdown");
    let embedding_pricing = kio_adapter::tool_lock::registered_declared_pricing("embedding");
    let (estimated_markdownize_usd, estimated_embedding_usd) =
        estimated_enrichment_cost_usd(&candidates, &markdownize_pricing, &embedding_pricing);
    Ok(ScanPreview {
        scope_id: scope_id_from_scope_json(&scope_path).unwrap_or_else(|| "unknown".to_owned()),
        candidates,
        estimated_cost: Some(CostPreview {
            estimated_usd: estimated_markdownize_usd + estimated_embedding_usd,
            budget_cap_usd: None,
            budget_warning: None,
            estimated_markdownize_usd,
            estimated_embedding_usd,
        }),
        approval_required: request.require_network_approval,
    })
}

/// Build a direct-file scan preview from retained scope and `.kio` directory
/// handles. This is the child-index counterpart to [`build_scan_preview`]: it
/// deliberately does not resolve `request.scope_path` after binding, so a
/// rename or symlink replacement of the public scope name cannot redirect a
/// source or policy read.
pub fn build_bound_scan_preview(
    root: &File,
    kio: &File,
    request: ScanPreviewRequest,
    inherited_rules: &[IgnoreRule],
) -> Result<ScanPreview> {
    let case_insensitive = probe_bound_case_insensitive(kio);
    let mut ignore_rules = inherited_rules.to_vec();
    ignore_rules.extend(load_bound_config_ignore(kio)?);
    ignore_rules.extend(load_bound_kioignore(root)?);
    let mut candidates = Vec::new();
    collect_bound_direct_candidates(
        root,
        &ignore_rules,
        request.include_raw_hashes,
        case_insensitive,
        &mut candidates,
    )?;
    apply_inherited_denials(&mut candidates, inherited_rules, case_insensitive)?;
    candidates.sort_by(|left, right| left.input_path.cmp(&right.input_path));
    let markdownize_pricing = kio_adapter::tool_lock::registered_declared_pricing("markdown");
    let embedding_pricing = kio_adapter::tool_lock::registered_declared_pricing("embedding");
    let (estimated_markdownize_usd, estimated_embedding_usd) =
        estimated_enrichment_cost_usd(&candidates, &markdownize_pricing, &embedding_pricing);
    Ok(ScanPreview {
        scope_id: bound_scope_id_from_scope_json(kio).unwrap_or_else(|| "unknown".to_owned()),
        candidates,
        estimated_cost: Some(CostPreview {
            estimated_usd: estimated_markdownize_usd + estimated_embedding_usd,
            budget_cap_usd: None,
            budget_warning: None,
            estimated_markdownize_usd,
            estimated_embedding_usd,
        }),
        approval_required: request.require_network_approval,
    })
}

/// Build a current-policy preview through retained management handles. Policy
/// eligibility is checked before a candidate's raw bytes are opened or hashed.
/// The request path is presentation-only; it cannot redirect this operation.
pub fn build_managed_scan_preview(
    binding: &ManagementBinding,
    request: ScanPreviewRequest,
) -> Result<ScanPreview> {
    let record = read_record(binding).map_err(management_pipeline_error)?;
    let policy = CurrentPolicyEvaluator::load(binding, record.case_insensitive)?;
    let mut candidates = Vec::new();
    for entry in cap_fs::read_base_dir(binding.root_handle()).pipeline_io(Path::new("."))? {
        let entry = entry.pipeline_io(Path::new("."))?;
        let name = match entry.file_name().into_string() {
            Ok(name) => name,
            Err(_) => continue,
        };
        let path = Path::new(&name);
        let metadata = cap_fs::stat(binding.root_handle(), path, cap_fs::FollowSymlinks::No)
            .pipeline_io(path)?;
        if !metadata.file_type().is_file() {
            continue;
        }
        let allowed = policy.allows_path(&name)?;
        let raw_hash = if allowed && request.include_raw_hashes {
            Some(
                hash_bound_verified_scan_input(binding.root_handle(), &name, metadata.len())?
                    .raw_hash,
            )
        } else {
            None
        };
        let secret = classify_secret(&name);
        let media_type = media_type_for_path(path).to_owned();
        candidates.push(ScanCandidate {
            input_path: name,
            media_type,
            size_bytes: metadata.len(),
            raw_hash,
            ignored: !allowed,
            quarantine_reason: match secret {
                Some(SecretTier::TierA) if !allowed => Some("secrets_tier_a_excluded".to_owned()),
                Some(SecretTier::TierA) => Some("secrets_tier_a_online_hold".to_owned()),
                Some(SecretTier::TierB) => Some("secrets_tier_b_warning".to_owned()),
                None => None,
            },
        });
    }
    candidates.sort_by(|left, right| left.input_path.cmp(&right.input_path));
    policy.revalidate()?;
    let markdownize_pricing = kio_adapter::tool_lock::registered_declared_pricing("markdown");
    let embedding_pricing = kio_adapter::tool_lock::registered_declared_pricing("embedding");
    let (estimated_markdownize_usd, estimated_embedding_usd) =
        estimated_enrichment_cost_usd(&candidates, &markdownize_pricing, &embedding_pricing);
    Ok(ScanPreview {
        scope_id: record.scope_id,
        candidates,
        estimated_cost: Some(CostPreview {
            estimated_usd: estimated_markdownize_usd + estimated_embedding_usd,
            budget_cap_usd: None,
            budget_warning: None,
            estimated_markdownize_usd,
            estimated_embedding_usd,
        }),
        approval_required: request.require_network_approval,
    })
}

/// Preview one newly discovered child without creating its `.kio` directory or
/// resolving its pathname again. The parent supplies its persisted filesystem
/// case capability; ancestor policy is the in-memory projection retained by
/// [`BoundPlannedChild`].
pub fn build_planned_child_scan_preview(
    child: &BoundPlannedChild,
    request: ScanPreviewRequest,
    case_insensitive: bool,
) -> Result<ScanPreview> {
    let local_rules = load_bound_kioignore(&child.root)?;
    let mut candidates = Vec::new();
    for entry in cap_fs::read_base_dir(&child.root).pipeline_io(Path::new("."))? {
        let entry = entry.pipeline_io(Path::new("."))?;
        let name = match entry.file_name().into_string() {
            Ok(name) => name,
            Err(_) => continue,
        };
        let path = Path::new(&name);
        let metadata =
            cap_fs::stat(&child.root, path, cap_fs::FollowSymlinks::No).pipeline_io(path)?;
        if !metadata.file_type().is_file() {
            continue;
        }
        // This ordering is intentional: a parent denial prevents all raw-byte
        // opens, even if a child `.kioignore` contains a matching negation.
        let ancestor_denied =
            rules_ignore_path(&name, false, &child.inherited_rules, case_insensitive)?;
        let local_denied = rules_ignore_path(&name, false, &local_rules, case_insensitive)?;
        let tier_a_denied = classify_secret(&name) == Some(SecretTier::TierA)
            && !rules_explicitly_unignore_path(&name, false, &local_rules, case_insensitive)?;
        let allowed = !ancestor_denied && !local_denied && !tier_a_denied;
        let raw_hash = if allowed && request.include_raw_hashes {
            Some(hash_bound_verified_scan_input(&child.root, &name, metadata.len())?.raw_hash)
        } else {
            None
        };
        let secret = classify_secret(&name);
        let media_type = media_type_for_path(path).to_owned();
        candidates.push(ScanCandidate {
            input_path: name,
            media_type,
            size_bytes: metadata.len(),
            raw_hash,
            ignored: !allowed,
            quarantine_reason: match secret {
                Some(SecretTier::TierA) if !allowed => Some("secrets_tier_a_excluded".to_owned()),
                Some(SecretTier::TierA) => Some("secrets_tier_a_online_hold".to_owned()),
                Some(SecretTier::TierB) => Some("secrets_tier_b_warning".to_owned()),
                None => None,
            },
        });
    }
    candidates.sort_by(|left, right| left.input_path.cmp(&right.input_path));
    let markdownize_pricing = kio_adapter::tool_lock::registered_declared_pricing("markdown");
    let embedding_pricing = kio_adapter::tool_lock::registered_declared_pricing("embedding");
    let (estimated_markdownize_usd, estimated_embedding_usd) =
        estimated_enrichment_cost_usd(&candidates, &markdownize_pricing, &embedding_pricing);
    Ok(ScanPreview {
        scope_id: "unmanaged-child".to_owned(),
        candidates,
        estimated_cost: Some(CostPreview {
            estimated_usd: estimated_markdownize_usd + estimated_embedding_usd,
            budget_cap_usd: None,
            budget_warning: None,
            estimated_markdownize_usd,
            estimated_embedding_usd,
        }),
        approval_required: request.require_network_approval,
    })
}

/// Live pre-send eligibility check. This has no existence or byte-read gate,
/// so callers can use it for current and historical names alike.
pub fn managed_scan_policy_allows_file(binding: &ManagementBinding, path: &str) -> Result<bool> {
    let record = read_record(binding).map_err(management_pipeline_error)?;
    let policy = CurrentPolicyEvaluator::load(binding, record.case_insensitive)?;
    let allowed = policy.allows_path(path)?;
    policy.revalidate()?;
    Ok(allowed)
}

fn management_pipeline_error(error: kio_core::KioError) -> crate::PipelineError {
    crate::PipelineError::contract("KIO-E-POLICY-MANAGEMENT-001", error.to_string())
}

fn apply_inherited_denials(
    candidates: &mut [ScanCandidate],
    inherited_rules: &[IgnoreRule],
    case_insensitive: bool,
) -> Result<()> {
    for candidate in candidates {
        if rules_ignore_path(
            &candidate.input_path,
            false,
            inherited_rules,
            case_insensitive,
        )? {
            candidate.ignored = true;
            candidate.raw_hash = None;
        }
    }
    Ok(())
}

fn collect_bound_direct_candidates(
    root: &File,
    ignore_rules: &[IgnoreRule],
    include_raw_hashes: bool,
    case_insensitive: bool,
    candidates: &mut Vec<ScanCandidate>,
) -> Result<()> {
    for entry in cap_fs::read_base_dir(root).pipeline_io(Path::new("."))? {
        let entry = entry.pipeline_io(Path::new("."))?;
        let name = match entry.file_name().into_string() {
            Ok(name) => name,
            Err(_) => continue,
        };
        if name == ".kio" || name == ".kioignore" {
            continue;
        }
        let path = Path::new(&name);
        let listed = cap_fs::stat(root, path, cap_fs::FollowSymlinks::No).pipeline_io(path)?;
        if !listed.file_type().is_file() {
            continue;
        }
        let mut size_bytes = listed.len();
        let secret = classify_secret(&name);
        let ignored = try_ignored_by_rules(&name, false, ignore_rules, case_insensitive)?
            || secret == Some(SecretTier::TierA)
                && !try_explicitly_unignored(&name, false, ignore_rules, case_insensitive)?;
        let quarantine_reason = match secret {
            Some(SecretTier::TierA) if ignored => Some("secrets_tier_a_excluded".to_owned()),
            Some(SecretTier::TierA) => Some("secrets_tier_a_online_hold".to_owned()),
            Some(SecretTier::TierB) => Some("secrets_tier_b_warning".to_owned()),
            None => None,
        };
        let raw_hash = if include_raw_hashes && !ignored {
            let (mut file, metadata) = open_bound_verified_regular_file(root, &name)?;
            size_bytes = metadata.len();
            let read_limit = metadata.len().checked_add(1).ok_or_else(|| {
                crate::PipelineError::contract(
                    "KIO-E-SCAN-INPUT-OVERSIZED-001",
                    format!("scan candidate is too large to hash: {name}"),
                )
            })?;
            let mut reader = (&mut file).take(read_limit);
            let raw_hash = hash_reader(&mut reader).pipeline_io(path)?;
            if reader.limit() == 0 {
                return Err(crate::PipelineError::contract(
                    "KIO-E-SCAN-INPUT-CHANGED-001",
                    format!("scan candidate grew while it was being hashed: {name}"),
                ));
            }
            ensure_bound_file_unchanged(&file, &metadata, path)?;
            Some(raw_hash)
        } else {
            None
        };
        let media_type = media_type_for_path(path).to_owned();
        candidates.push(ScanCandidate {
            input_path: name,
            media_type,
            size_bytes,
            raw_hash,
            ignored,
            quarantine_reason,
        });
    }
    Ok(())
}

/// Find bounded recursive child scopes for a parent index invocation. Parent
/// files remain direct-only; each file-bearing directory is a separate scope.
/// `.kio` is pruned before ignore evaluation, directory symlinks are never
/// followed, and VCS roots prune their complete subtree unless opted in.
pub fn discover_child_scopes(scope_path: &Path) -> Result<ChildScopePlan> {
    let case_insensitive = probe_case_insensitive(scope_path);
    let mut rules = load_config_ignore(scope_path)?;
    rules.extend(load_kioignore(scope_path)?);
    let index_vcs_repos = load_index_vcs_repos(scope_path)?;
    let root_handle = cap_fs::open_ambient_dir(scope_path, ambient_authority()).map_err(|err| {
        crate::PipelineError::Io {
            path: scope_path.display().to_string(),
            message: err.to_string(),
        }
    })?;
    if !index_vcs_repos && is_vcs_root(&root_handle) {
        let mut row = child_status(String::new(), "skipped_vcs");
        row.reason = Some("scope_root_is_vcs".to_owned());
        return Ok(ChildScopePlan {
            candidates: vec![row],
            root_handle,
            planned_handles: BTreeMap::new(),
            canonical_roots: BTreeMap::new(),
            index_vcs_repos,
            effective_ignore_rules: rules,
        });
    }
    let mut result = Vec::new();
    let mut planned_handles = BTreeMap::new();
    let mut canonical_roots = BTreeMap::new();
    let mut visited = 0;
    discover_child_scopes_inner(
        scope_path,
        &root_handle,
        Path::new(""),
        0,
        &rules,
        case_insensitive,
        index_vcs_repos,
        &mut result,
        &mut planned_handles,
        &mut canonical_roots,
        &mut visited,
    )?;
    result.sort_by(|a, b| a.path.cmp(&b.path).then(a.status.cmp(&b.status)));
    let plan = ChildScopePlan {
        candidates: result,
        root_handle,
        planned_handles,
        canonical_roots,
        index_vcs_repos,
        effective_ignore_rules: rules,
    };
    Ok(plan)
}

/// Retained-handle managed discovery. The filesystem case capability and every
/// ancestor rule come from live management/policy authority; this path never
/// creates a probe file or reopens the scope by its public pathname.
pub fn discover_managed_child_scopes(binding: &ManagementBinding) -> Result<ChildScopePlan> {
    let record = read_record(binding).map_err(management_pipeline_error)?;
    let policy = CurrentPolicyEvaluator::load(binding, record.case_insensitive)?;
    if !policy.allows_scope()? {
        return Err(crate::PipelineError::contract(
            "KIO-E-POLICY-SCOPE-DENIED-001",
            "managed scope is denied by current ancestor policy",
        ));
    }
    let rules = policy.projected_rules_for_child("")?;
    let index_vcs_repos = load_bound_index_vcs_repos(binding.kio_handle())?;
    let root_handle =
        binding
            .root_handle()
            .try_clone()
            .map_err(|error| crate::PipelineError::Io {
                path: binding.canonical_root().display().to_string(),
                message: error.to_string(),
            })?;
    if !index_vcs_repos && is_vcs_root(&root_handle) {
        let mut row = child_status(String::new(), "skipped_vcs");
        row.reason = Some("scope_root_is_vcs".to_owned());
        return Ok(ChildScopePlan {
            candidates: vec![row],
            root_handle,
            planned_handles: BTreeMap::new(),
            canonical_roots: BTreeMap::new(),
            index_vcs_repos,
            effective_ignore_rules: rules,
        });
    }
    let mut result = Vec::new();
    let mut planned_handles = BTreeMap::new();
    let mut canonical_roots = BTreeMap::new();
    let mut visited = 0;
    discover_child_scopes_inner(
        binding.canonical_root(),
        &root_handle,
        Path::new(""),
        0,
        &rules,
        record.case_insensitive,
        index_vcs_repos,
        &mut result,
        &mut planned_handles,
        &mut canonical_roots,
        &mut visited,
    )?;
    result.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then(left.status.cmp(&right.status))
    });
    policy.revalidate()?;
    Ok(ChildScopePlan {
        candidates: result,
        root_handle,
        planned_handles,
        canonical_roots,
        index_vcs_repos,
        effective_ignore_rules: rules,
    })
}

/// Discover one bounded, resumable page of managed child scopes.
///
/// This is deliberately not a wrapper around [`discover_managed_child_scopes`]:
/// it uses an iterative DFS frontier and only retains the current depth plus a
/// 129-name directory page. Unlike the legacy one-shot entrypoint, this
/// continuation has no depth cap; token bytes and OS path admission are the
/// bounded frontier. A continuation is invalidated on root identity,
/// management/policy, VCS-policy, or retained-frame identity drift; callers
/// must restart at the root rather than using stale traversal authority.
pub fn discover_managed_child_scopes_page(
    binding: &ManagementBinding,
    continuation: Option<&ChildScopeDiscoveryContinuation>,
) -> Result<ChildScopeDiscoverySlice> {
    let record = read_record(binding).map_err(management_pipeline_error)?;
    let policy = CurrentPolicyEvaluator::load(binding, record.case_insensitive)?;
    if !policy.allows_scope()? {
        return Err(crate::PipelineError::contract(
            "KIO-E-POLICY-SCOPE-DENIED-001",
            "managed scope is denied by current ancestor policy",
        ));
    }
    let rules = policy.projected_rules_for_child("")?;
    // Enrolling a child mutates ManagementRecord.children, which is not policy
    // authority. Hash the effective rules/case mode instead, while fresh
    // CurrentPolicyEvaluator loading still validates the live chain per page.
    let discovery_policy_digest = child_discovery_policy_digest(&rules, record.case_insensitive)?;
    let index_vcs_repos = load_bound_index_vcs_repos(binding.kio_handle())?;
    let root_handle =
        binding
            .root_handle()
            .try_clone()
            .map_err(|error| crate::PipelineError::Io {
                path: binding.canonical_root().display().to_string(),
                message: error.to_string(),
            })?;
    let root_identity = directory_identity(&root_handle)?;
    let mut frames = match continuation {
        Some(token) => {
            validate_discovery_continuation(
                token,
                &record.scope_id,
                &root_identity,
                record.case_insensitive,
                &discovery_policy_digest,
                index_vcs_repos,
            )?;
            token.frames.clone()
        }
        None => vec![ChildScopeDiscoveryFrame {
            relative_dir: String::new(),
            expected_identity: root_identity.clone(),
            after_entry: None,
            pending_entries: Vec::new(),
        }],
    };
    if !index_vcs_repos && is_vcs_root(&root_handle) {
        let mut row = child_status(String::new(), "skipped_vcs");
        row.reason = Some("scope_root_is_vcs".to_owned());
        return Ok(ChildScopeDiscoverySlice {
            plan: empty_child_scope_plan(root_handle, index_vcs_repos, rules, vec![row]),
            continuation: None,
        });
    }

    let mut result = Vec::new();
    let mut planned_handles = BTreeMap::new();
    let mut canonical_roots = BTreeMap::new();
    while result.len() < CHILD_SCOPE_DISCOVERY_PAGE_SIZE && !frames.is_empty() {
        let index = frames.len() - 1;
        let directory = reopen_continuation_directory(&root_handle, &frames[index])?;
        if frames[index].pending_entries.is_empty() {
            frames[index].pending_entries =
                select_discovery_entry_page(&directory, frames[index].after_entry.as_deref())?;
        }
        let Some(name) = frames[index].pending_entries.first().cloned() else {
            frames.pop();
            continue;
        };
        frames[index].pending_entries.remove(0);
        frames[index].after_entry = Some(name.clone());
        if name == ".kio" {
            continue;
        }
        let relative_path = if frames[index].relative_dir.is_empty() {
            PathBuf::from(&name)
        } else {
            Path::new(&frames[index].relative_dir).join(&name)
        };
        validate_discovery_relative(&relative_path)?;
        let relative = relative_scope_path(&relative_path);
        let metadata = match cap_fs::stat(&directory, Path::new(&name), cap_fs::FollowSymlinks::No)
        {
            Ok(metadata) => metadata,
            Err(_) => {
                let mut row = child_status(relative, "skipped_unreadable");
                row.reason = Some("changed_or_unreadable".to_owned());
                result.push(row);
                continue;
            }
        };
        if metadata.file_type().is_symlink() {
            result.push(child_status(relative, "skipped_symlink"));
            continue;
        }
        if !metadata.file_type().is_dir() {
            continue;
        }
        if is_xdg_state_inside_scope(
            binding.canonical_root(),
            &binding.canonical_root().join(&relative_path),
        ) {
            result.push(child_status(relative, "skipped_xdg_state"));
            continue;
        }
        if try_ignored_by_rules(&relative, true, &rules, record.case_insensitive)? {
            result.push(child_status(relative, "skipped_ignored"));
            continue;
        }
        let child = match cap_fs::open_dir_nofollow(&directory, Path::new(&name)) {
            Ok(child) => child,
            Err(_) => {
                let mut row = child_status(relative, "skipped_unreadable");
                row.reason = Some("changed_or_unreadable".to_owned());
                result.push(row);
                continue;
            }
        };
        #[cfg(windows)]
        if kio_core::cas::windows_directory_handle_identity(&child).is_none() {
            let mut row = child_status(relative, "skipped_symlink");
            row.reason = Some("windows_reparse_point".to_owned());
            result.push(row);
            continue;
        }
        if !same_cap_directory_volume(&directory, &child)? {
            let mut row = child_status(relative, "skipped_mount");
            row.reason = Some("independent_root_required_for_volume".to_owned());
            result.push(row);
            continue;
        }
        if !index_vcs_repos && is_vcs_root(&child) {
            result.push(child_status(relative, "skipped_vcs"));
            continue;
        }
        result.push(child_status(relative.clone(), "planned"));
        planned_handles.insert(
            relative.clone(),
            child
                .try_clone()
                .map_err(|error| crate::PipelineError::Io {
                    path: relative.clone(),
                    message: error.to_string(),
                })?,
        );
        canonical_roots.insert(
            relative.clone(),
            binding.canonical_root().join(&relative_path),
        );
        frames.push(ChildScopeDiscoveryFrame {
            relative_dir: relative,
            expected_identity: directory_identity(&child)?,
            after_entry: None,
            pending_entries: Vec::new(),
        });
    }
    policy.revalidate()?;
    let continuation = if frames.is_empty() {
        None
    } else {
        let token = ChildScopeDiscoveryContinuation {
            version: CHILD_SCOPE_DISCOVERY_TOKEN_VERSION,
            scope_id: record.scope_id,
            root_identity,
            case_insensitive: record.case_insensitive,
            policy_digest: discovery_policy_digest,
            index_vcs_repos,
            frames,
        };
        validate_discovery_token_size(&token)?;
        Some(token)
    };
    Ok(ChildScopeDiscoverySlice {
        plan: ChildScopePlan {
            candidates: result,
            root_handle,
            planned_handles,
            canonical_roots,
            index_vcs_repos,
            effective_ignore_rules: rules,
        },
        continuation,
    })
}

fn child_discovery_policy_digest(rules: &[IgnoreRule], case_insensitive: bool) -> Result<String> {
    let bytes = serde_json::to_vec(&(case_insensitive, rules))
        .map_err(|error| crate::PipelineError::Schema(error.to_string()))?;
    Ok(hash_bytes(&bytes))
}

fn empty_child_scope_plan(
    root_handle: File,
    index_vcs_repos: bool,
    rules: Vec<IgnoreRule>,
    candidates: Vec<ChildScopeDiscovery>,
) -> ChildScopePlan {
    ChildScopePlan {
        candidates,
        root_handle,
        planned_handles: BTreeMap::new(),
        canonical_roots: BTreeMap::new(),
        index_vcs_repos,
        effective_ignore_rules: rules,
    }
}

fn validate_discovery_continuation(
    token: &ChildScopeDiscoveryContinuation,
    scope_id: &str,
    root_identity: &DirectoryIdentity,
    case_insensitive: bool,
    policy_digest: &str,
    index_vcs_repos: bool,
) -> Result<()> {
    validate_discovery_token_size(token)?;
    if token.version != CHILD_SCOPE_DISCOVERY_TOKEN_VERSION
        || token.scope_id != scope_id
        || &token.root_identity != root_identity
        || token.case_insensitive != case_insensitive
        || token.policy_digest != policy_digest
        || token.index_vcs_repos != index_vcs_repos
    {
        return Err(crate::PipelineError::contract(
            "KIO-E-CHILD-DISCOVERY-CONTINUATION-STALE-001",
            "child discovery continuation no longer matches live root identity or policy; restart from the root",
        ));
    }
    validate_discovery_token_syntax(token)?;
    Ok(())
}

/// Parse an untrusted persisted continuation after enforcing the allocation
/// bound. App storage should use this rather than deserializing the token
/// directly.
pub fn parse_child_scope_discovery_continuation(
    bytes: &[u8],
) -> Result<ChildScopeDiscoveryContinuation> {
    if bytes.len() > MAX_CHILD_SCOPE_DISCOVERY_TOKEN_BYTES {
        return Err(crate::PipelineError::contract(
            "KIO-E-CHILD-DISCOVERY-CONTINUATION-STALE-001",
            "child discovery continuation exceeds its bounded token size",
        ));
    }
    let token = serde_json::from_slice(bytes).map_err(|error| {
        crate::PipelineError::contract(
            "KIO-E-CHILD-DISCOVERY-CONTINUATION-STALE-001",
            format!("invalid child discovery continuation: {error}"),
        )
    })?;
    validate_discovery_token_size(&token)?;
    validate_discovery_token_syntax(&token)?;
    Ok(token)
}

fn is_discovery_entry_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\\')
}

fn validate_discovery_token_syntax(token: &ChildScopeDiscoveryContinuation) -> Result<()> {
    if token.frames.is_empty() || !token.frames[0].relative_dir.is_empty() {
        return Err(crate::PipelineError::contract(
            "KIO-E-CHILD-DISCOVERY-CONTINUATION-STALE-001",
            "child discovery continuation has an invalid frontier",
        ));
    }
    let mut parent = PathBuf::new();
    let mut seen = BTreeSet::new();
    for frame in &token.frames {
        let path = Path::new(&frame.relative_dir);
        validate_discovery_relative(path)?;
        if !seen.insert(frame.relative_dir.as_str())
            || (!parent.as_os_str().is_empty() && !path.starts_with(&parent))
        {
            return Err(crate::PipelineError::contract(
                "KIO-E-CHILD-DISCOVERY-CONTINUATION-STALE-001",
                "child discovery continuation has duplicate or unordered frames",
            ));
        }
        if frame.pending_entries.len() > CHILD_SCOPE_DISCOVERY_SELECTION_SIZE
            || !frame
                .pending_entries
                .windows(2)
                .all(|pair| pair[0] < pair[1])
            || frame.pending_entries.iter().any(|name| {
                !is_discovery_entry_name(name)
                    || frame
                        .after_entry
                        .as_ref()
                        .is_some_and(|after| name <= after)
            })
            || frame
                .after_entry
                .as_ref()
                .is_some_and(|name| !is_discovery_entry_name(name))
        {
            return Err(crate::PipelineError::contract(
                "KIO-E-CHILD-DISCOVERY-CONTINUATION-STALE-001",
                "child discovery continuation has an invalid directory page",
            ));
        }
        parent = path.to_path_buf();
    }
    Ok(())
}

fn validate_discovery_token_size(token: &ChildScopeDiscoveryContinuation) -> Result<()> {
    let bytes = serde_json::to_vec(token)
        .map_err(|error| crate::PipelineError::Schema(error.to_string()))?;
    if bytes.len() > MAX_CHILD_SCOPE_DISCOVERY_TOKEN_BYTES {
        return Err(crate::PipelineError::contract(
            "KIO-E-CHILD-DISCOVERY-CONTINUATION-STALE-001",
            "child discovery continuation exceeds its bounded token size",
        ));
    }
    Ok(())
}

fn validate_discovery_relative(path: &Path) -> Result<()> {
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
        && !path.as_os_str().is_empty()
    {
        return Err(crate::PipelineError::contract(
            "KIO-E-CHILD-DISCOVERY-CONTINUATION-STALE-001",
            "child discovery continuation contains a non-relative path",
        ));
    }
    #[cfg(windows)]
    if path.as_os_str().encode_wide().count() > MAX_CHILD_SCOPE_DISCOVERY_RELATIVE_UTF16 {
        return Err(crate::PipelineError::contract(
            "KIO-E-CHILD-DISCOVERY-CONTINUATION-STALE-001",
            "child discovery path exceeds the Windows supported limit",
        ));
    }
    #[cfg(not(windows))]
    if path.as_os_str().as_encoded_bytes().len() > MAX_CHILD_SCOPE_DISCOVERY_RELATIVE_BYTES {
        return Err(crate::PipelineError::contract(
            "KIO-E-CHILD-DISCOVERY-CONTINUATION-STALE-001",
            "child discovery path exceeds the supported token limit",
        ));
    }
    Ok(())
}

fn reopen_continuation_directory(root: &File, frame: &ChildScopeDiscoveryFrame) -> Result<File> {
    let mut current = root.try_clone().map_err(|error| crate::PipelineError::Io {
        path: frame.relative_dir.clone(),
        message: error.to_string(),
    })?;
    for component in Path::new(&frame.relative_dir).components() {
        let Component::Normal(component) = component else {
            continue;
        };
        current = cap_fs::open_dir_nofollow(&current, Path::new(component)).map_err(|error| {
            crate::PipelineError::contract(
                "KIO-E-CHILD-DISCOVERY-CONTINUATION-STALE-001",
                format!(
                    "cannot rebind discovery frontier {}: {error}",
                    frame.relative_dir
                ),
            )
        })?;
    }
    if directory_identity(&current)? != frame.expected_identity {
        return Err(crate::PipelineError::contract(
            "KIO-E-CHILD-DISCOVERY-CONTINUATION-STALE-001",
            "child discovery frontier directory identity changed; restart from the root",
        ));
    }
    Ok(current)
}

fn select_discovery_entry_page(dir: &File, after: Option<&str>) -> Result<Vec<String>> {
    let mut names = BTreeSet::new();
    for entry in cap_fs::read_base_dir(dir).pipeline_io(Path::new("."))? {
        let entry = entry.pipeline_io(Path::new("."))?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if after.is_some_and(|cursor| name.as_str() <= cursor) {
            continue;
        }
        names.insert(name);
        if names.len() > CHILD_SCOPE_DISCOVERY_SELECTION_SIZE {
            names.pop_last();
        }
    }
    Ok(names.into_iter().collect())
}

#[cfg(unix)]
fn directory_identity(file: &File) -> Result<DirectoryIdentity> {
    use cap_fs::MetadataExt;
    let metadata = cap_fs::Metadata::from_file(file).pipeline_io(Path::new("."))?;
    if !metadata.file_type().is_dir() {
        return Err(crate::PipelineError::contract(
            "KIO-E-CHILD-DISCOVERY-CONTINUATION-STALE-001",
            "discovery frontier is not a directory",
        ));
    }
    Ok(DirectoryIdentity::Unix {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}
#[cfg(windows)]
fn directory_identity(file: &File) -> Result<DirectoryIdentity> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        GetFileInformationByHandle,
    };
    let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut information) } == 0
        || information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
        || information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err(crate::PipelineError::contract(
            "KIO-E-CHILD-DISCOVERY-CONTINUATION-STALE-001",
            "discovery frontier is not a real directory",
        ));
    }
    Ok(DirectoryIdentity::Windows {
        volume_serial_number: information.dwVolumeSerialNumber,
        file_index: ((information.nFileIndexHigh as u64) << 32) | information.nFileIndexLow as u64,
    })
}
#[cfg(not(any(unix, windows)))]
fn directory_identity(_file: &File) -> Result<DirectoryIdentity> {
    Err(crate::PipelineError::contract(
        "KIO-E-SCAN-DIRECTORY-IDENTITY-001",
        "directory identities are unsupported on this platform",
    ))
}

fn load_bound_index_vcs_repos(kio: &File) -> Result<bool> {
    let Some(text) = read_bound_optional_regular_text(kio, "config.toml")? else {
        return Ok(false);
    };
    let value: toml::Value =
        toml::from_str(&text).map_err(|error| crate::PipelineError::Schema(error.to_string()))?;
    match value
        .get("scope")
        .and_then(|scope| scope.get("index_vcs_repos"))
    {
        None => Ok(false),
        Some(value) => value.as_bool().ok_or_else(|| {
            crate::PipelineError::Schema("scope.index_vcs_repos must be boolean".to_owned())
        }),
    }
}

/// Rebind a planned child immediately before direct execution. The parent
/// discovery handle is reopened without following links and compared with its
/// discovery-time identity. A VCS marker is also checked after rebinding;
/// `None` means the child must remain excluded.
pub fn bind_planned_child(
    plan: &ChildScopePlan,
    relative: &str,
) -> Result<Option<BoundPlannedChild>> {
    let root = bound_child_handle(plan, relative)?;
    if !plan.index_vcs_repos && is_vcs_root(&root) {
        return Ok(None);
    }
    let canonical_root = plan.canonical_roots.get(relative).cloned().ok_or_else(|| {
        crate::PipelineError::Schema(format!("unknown planned child scope: {relative}"))
    })?;
    let inherited_rules = generated_parent_policy_for_child(plan, relative)?;
    Ok(Some(BoundPlannedChild {
        canonical_root,
        root,
        inherited_rules,
    }))
}

fn bound_child_handle(plan: &ChildScopePlan, relative: &str) -> Result<File> {
    let expected = plan.planned_handles.get(relative).ok_or_else(|| {
        crate::PipelineError::Schema(format!("unknown planned child scope: {relative}"))
    })?;
    let mut current = plan
        .root_handle
        .try_clone()
        .map_err(|err| crate::PipelineError::Io {
            path: relative.to_owned(),
            message: err.to_string(),
        })?;
    for component in Path::new(relative).components() {
        let Component::Normal(component) = component else {
            return Err(crate::PipelineError::Schema(format!(
                "invalid planned child scope: {relative}"
            )));
        };
        current = cap_fs::open_dir_nofollow(&current, Path::new(component)).map_err(|err| {
            crate::PipelineError::Io {
                path: relative.to_owned(),
                message: err.to_string(),
            }
        })?;
    }
    let expected =
        cap_fs::Metadata::from_file(expected).map_err(|err| crate::PipelineError::Io {
            path: relative.to_owned(),
            message: err.to_string(),
        })?;
    let actual = cap_fs::Metadata::from_file(&current).map_err(|err| crate::PipelineError::Io {
        path: relative.to_owned(),
        message: err.to_string(),
    })?;
    if !same_cap_directory_identity(&expected, &actual) {
        return Err(crate::PipelineError::Schema(format!(
            "planned child scope changed during discovery: {relative}"
        )));
    }
    Ok(current)
}

#[allow(clippy::too_many_arguments)] // retained directory handle + traversal state are security-relevant inputs
fn discover_child_scopes_inner(
    root: &Path,
    dir: &File,
    relative_dir: &Path,
    depth: usize,
    rules: &[IgnoreRule],
    case_insensitive: bool,
    index_vcs_repos: bool,
    result: &mut Vec<ChildScopeDiscovery>,
    planned_handles: &mut BTreeMap<String, File>,
    canonical_roots: &mut BTreeMap<String, PathBuf>,
    visited: &mut usize,
) -> Result<()> {
    if depth >= MAX_CHILD_SCOPE_DEPTH {
        return Ok(());
    }
    let mut entries = match cap_fs::read_base_dir(dir).and_then(|entries| {
        // Stream arbitrary ordinary files without retaining them; only directory
        // candidates consume discovery capacity/allocation.
        let mut directories = Vec::with_capacity(MAX_CHILD_SCOPE_DIRECTORIES + 1);
        for entry in entries {
            let entry = entry?;
            if entry.file_type()?.is_dir() || entry.file_type()?.is_symlink() {
                directories.push(entry);
                if directories.len() > MAX_CHILD_SCOPE_DIRECTORIES {
                    break;
                }
            }
        }
        Ok(directories)
    }) {
        Ok(entries) => entries,
        Err(_) if !relative_dir.as_os_str().is_empty() => {
            let mut row = child_status(relative_scope_path(relative_dir), "skipped_unreadable");
            row.reason = Some("read_dir_failed".to_owned());
            result.push(row);
            return Ok(());
        }
        Err(err) => {
            return Err(crate::PipelineError::Io {
                path: root.display().to_string(),
                message: err.to_string(),
            });
        }
    };
    if entries.len() > MAX_CHILD_SCOPE_DIRECTORIES {
        let mut row = child_status(relative_scope_path(relative_dir), "skipped_limit");
        row.reason = Some("directory_entry_cap".to_owned());
        result.push(row);
        return Ok(());
    }
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name();
        if name == ".kio" {
            continue;
        }
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };
        let relative_path = relative_dir.join(&name);
        let relative = relative_scope_path(&relative_path);
        if file_type.is_symlink() {
            result.push(child_status(relative, "skipped_symlink"));
            continue;
        }
        if !file_type.is_dir() {
            continue;
        }
        if is_xdg_state_inside_scope(root, &root.join(&relative_path)) {
            result.push(child_status(relative, "skipped_xdg_state"));
            continue;
        }
        if *visited >= MAX_CHILD_SCOPE_DIRECTORIES {
            let mut row = child_status(relative, "skipped_limit");
            row.reason = Some("directory_cap".to_owned());
            result.push(row);
            continue;
        }
        *visited += 1;
        if try_ignored_by_rules(&relative, true, rules, case_insensitive)? {
            result.push(child_status(relative, "skipped_ignored"));
            continue;
        }
        let child = match cap_fs::open_dir_nofollow(dir, Path::new(&name)) {
            Ok(child) => child,
            Err(_) => {
                let mut row = child_status(relative, "skipped_unreadable");
                row.reason = Some("changed_or_unreadable".to_owned());
                result.push(row);
                continue;
            }
        };
        // `open_dir_nofollow` gives us the capability that all later probes
        // and recursion use. On Windows, reject a handle for anything other
        // than a real directory here: a junction/reparse point is never a
        // child-scope candidate, and must not be followed by discovery.
        #[cfg(windows)]
        if kio_core::cas::windows_directory_handle_identity(&child).is_none() {
            let mut row = child_status(relative, "skipped_symlink");
            row.reason = Some("windows_reparse_point".to_owned());
            result.push(row);
            continue;
        }
        if !same_cap_directory_volume(dir, &child)? {
            let mut row = child_status(relative, "skipped_mount");
            row.reason = Some("independent_root_required_for_volume".to_owned());
            result.push(row);
            continue;
        }
        if !index_vcs_repos && is_vcs_root(&child) {
            result.push(child_status(relative, "skipped_vcs"));
            continue;
        }
        // Management covers directories themselves, including empty folders
        // and folders whose current files are all ignored. File eligibility is
        // evaluated later by the managed scan, never used as enrollment proof.
        result.push(child_status(relative.clone(), "planned"));
        planned_handles.insert(
            relative.clone(),
            child.try_clone().map_err(|err| crate::PipelineError::Io {
                path: relative.clone(),
                message: err.to_string(),
            })?,
        );
        canonical_roots.insert(relative.clone(), root.join(&relative_path));
        if depth + 1 >= MAX_CHILD_SCOPE_DEPTH {
            let mut row = child_status(relative, "skipped_limit");
            row.reason = Some("depth_cap".to_owned());
            result.push(row);
            continue;
        }
        discover_child_scopes_inner(
            root,
            &child,
            &relative_path,
            depth + 1,
            rules,
            case_insensitive,
            index_vcs_repos,
            result,
            planned_handles,
            canonical_roots,
            visited,
        )?;
    }
    Ok(())
}

fn relative_scope_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn same_cap_directory_volume(left: &File, right: &File) -> Result<bool> {
    let left = cap_fs::Metadata::from_file(left).pipeline_io(Path::new("."))?;
    let right = cap_fs::Metadata::from_file(right).pipeline_io(Path::new("."))?;
    #[cfg(unix)]
    {
        use cap_fs::MetadataExt;
        Ok(left.dev() == right.dev())
    }
    #[cfg(windows)]
    {
        use cap_fs::_WindowsByHandle;
        Ok(left.volume_serial_number() == right.volume_serial_number())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (left, right);
        Err(crate::PipelineError::contract(
            "KIO-E-SCAN-VOLUME-001",
            "platform has no retained filesystem identity",
        ))
    }
}

fn is_vcs_root(path: &File) -> bool {
    cap_fs::stat(path, Path::new(".git"), cap_fs::FollowSymlinks::No).is_ok()
        || [".hg", ".svn", ".bzr"]
            .iter()
            .any(|marker| cap_fs::stat(path, Path::new(marker), cap_fs::FollowSymlinks::No).is_ok())
}

#[cfg(unix)]
fn same_cap_directory_identity(left: &cap_fs::Metadata, right: &cap_fs::Metadata) -> bool {
    use cap_fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}
#[cfg(windows)]
fn same_cap_directory_identity(left: &cap_fs::Metadata, right: &cap_fs::Metadata) -> bool {
    use cap_fs::_WindowsByHandle;
    left.volume_serial_number() == right.volume_serial_number()
        && left.file_index() == right.file_index()
}
#[cfg(not(any(unix, windows)))]
fn same_cap_directory_identity(left: &cap_fs::Metadata, right: &cap_fs::Metadata) -> bool {
    left.len() == right.len() && left.modified().ok() == right.modified().ok()
}

pub fn load_index_vcs_repos(scope_path: &Path) -> Result<bool> {
    let path = scope_path.join(".kio/config.toml");
    let Some(text) = read_optional_bounded_local_text(&path)? else {
        return Ok(false);
    };
    let value: toml::Value =
        toml::from_str(&text).map_err(|err| crate::PipelineError::Schema(err.to_string()))?;
    let Some(value) = value
        .get("scope")
        .and_then(|scope| scope.get("index_vcs_repos"))
    else {
        return Ok(false);
    };
    value.as_bool().ok_or_else(|| {
        crate::PipelineError::Schema("scope.index_vcs_repos must be a boolean".to_owned())
    })
}

/// QA19 (step4b-contract-tests-p3a.md §F, 10 §1 L48-53, 07 §4 L298-303):
/// "コスト概算は…tools.toml の [pricing] 単価表 × 推定ページ数/トークン数から算出
/// する桁の目安" — the declared unit price (`markdownize_pricing`/
/// `embedding_pricing`, sourced by the caller from `kio-adapter`'s
/// `tool_lock::registered_declared_pricing`) times a rough page/token count
/// derived from candidate byte sizes. Takes pricing as parameters (rather
/// than reading the process-global registry itself) so this arithmetic is
/// unit-testable without depending on `tool_lock`'s once-per-process
/// registration. Deliberately independent of `kio-cli`'s task-execution
/// reservation heuristics (`estimate_online_markdownize_cost`/
/// `estimate_embedding_cost`, which stay untouched — this function does not
/// gate any spend, only a pre-approval display figure the spec itself calls
/// "a ballpark, not a guarantee"). Returns `(markdownize_usd, embedding_usd)`;
/// either is `0.0` when its role has no declared `pricing` — an honest
/// "unknown", not a fabricated number.
fn estimated_enrichment_cost_usd(
    candidates: &[ScanCandidate],
    markdownize_pricing: &BTreeMap<String, f64>,
    embedding_pricing: &BTreeMap<String, f64>,
) -> (f64, f64) {
    // Mirrors `kio-cli`'s `is_text_native_media` exactly (docs/04 §2: text-
    // native files skip Markdownize/OCR and are indexed as-is).
    const TEXT_NATIVE_MEDIA_TYPES: [&str; 3] = ["text/markdown", "text/plain", "text/x-code"];
    // Rough, order-of-magnitude assumptions for THIS preview estimate only
    // (10 §1's own framing — not a precision requirement).
    const ESTIMATED_BYTES_PER_PAGE: f64 = 3_000.0;
    const ESTIMATED_BYTES_PER_TOKEN: f64 = 4.0;

    let included: Vec<&ScanCandidate> = candidates.iter().filter(|c| !c.ignored).collect();
    let markdownize_bytes: u64 = included
        .iter()
        .filter(|c| !TEXT_NATIVE_MEDIA_TYPES.contains(&c.media_type.as_str()))
        .map(|c| c.size_bytes)
        .sum();
    let embedding_bytes: u64 = included.iter().map(|c| c.size_bytes).sum();

    let estimated_markdownize_usd = markdownize_pricing.get("pages").copied().unwrap_or(0.0)
        * (markdownize_bytes as f64 / ESTIMATED_BYTES_PER_PAGE).ceil();
    let estimated_embedding_usd = embedding_pricing.get("tokens_in").copied().unwrap_or(0.0)
        * (embedding_bytes as f64 / ESTIMATED_BYTES_PER_TOKEN);
    (estimated_markdownize_usd, estimated_embedding_usd)
}

fn collect_direct_candidates(
    scope_path: &Path,
    ignore_rules: &[IgnoreRule],
    include_raw_hashes: bool,
    case_insensitive: bool,
    candidates: &mut Vec<ScanCandidate>,
) -> Result<()> {
    for entry in std::fs::read_dir(scope_path).pipeline_io(scope_path)? {
        let entry = entry.pipeline_io(scope_path)?;
        let name = match entry.file_name().into_string() {
            Ok(name) => name,
            Err(_) => continue,
        };
        if name == ".kio" || name == ".kioignore" {
            continue;
        }
        let path = entry.path();
        if is_xdg_state_inside_scope(scope_path, &path) {
            continue;
        }
        let file_type = entry.file_type().pipeline_io(&path)?;
        if !file_type.is_file() {
            continue;
        }
        let relative = path
            .strip_prefix(scope_path)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if relative == ".kioignore" {
            continue;
        }
        let mut size_bytes = entry.metadata().pipeline_io(&path)?.len();
        let secret = classify_secret(&relative);
        let ignored = try_ignored_by_rules(
            &relative,
            file_type.is_dir(),
            ignore_rules,
            case_insensitive,
        )? || secret == Some(SecretTier::TierA)
            && !try_explicitly_unignored(
                &relative,
                file_type.is_dir(),
                ignore_rules,
                case_insensitive,
            )?;
        let quarantine_reason = match secret {
            Some(SecretTier::TierA) if ignored => Some("secrets_tier_a_excluded".to_owned()),
            // R19-1: a Tier A secret explicitly un-ignored (`!pattern`) is ingested
            // locally but MUST still be held from online send like Tier B — the lift
            // approves local management, not cloud upload (10 §1.1: the mechanism
            // prevents "オンライン送信事故"). This marker drives the audit record; the
            // send-blocking gates key on `classify_secret` directly.
            Some(SecretTier::TierA) => Some("secrets_tier_a_online_hold".to_owned()),
            Some(SecretTier::TierB) => Some("secrets_tier_b_warning".to_owned()),
            _ => None,
        };
        let raw_hash = if include_raw_hashes && !ignored {
            let (mut file, metadata) = open_verified_regular_file(&path)?;
            size_bytes = metadata.len();
            let read_limit = metadata.len().checked_add(1).ok_or_else(|| {
                crate::PipelineError::contract(
                    "KIO-E-SCAN-INPUT-OVERSIZED-001",
                    format!("scan candidate is too large to hash: {}", path.display()),
                )
            })?;
            let mut reader = (&mut file).take(read_limit);
            let raw_hash = hash_reader(&mut reader).pipeline_io(&path)?;
            if reader.limit() == 0 {
                return Err(crate::PipelineError::contract(
                    "KIO-E-SCAN-INPUT-CHANGED-001",
                    format!(
                        "scan candidate grew while it was being hashed: {}",
                        path.display()
                    ),
                ));
            }
            ensure_file_unchanged(&file, &metadata, &path)?;
            Some(raw_hash)
        } else {
            None
        };
        candidates.push(ScanCandidate {
            input_path: relative.clone(),
            media_type: media_type_for_path(&path).to_owned(),
            size_bytes,
            raw_hash,
            ignored,
            quarantine_reason,
        });
    }
    Ok(())
}

/// A direct-child input read through one verified file handle and bounded before
/// allocation. The returned hash always identifies the returned bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedScanInput {
    pub bytes: Vec<u8>,
    pub raw_hash: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedScanIdentity {
    pub raw_hash: String,
    pub size_bytes: u64,
}

/// Open and read a scope-local direct child without trusting a pathname check for
/// a later pathname read. The metadata size is checked before allocation, and a
/// bounded reader catches growth after that check.
pub fn read_verified_scan_input(
    scope_path: &Path,
    input_path: &str,
    max_bytes: u64,
) -> Result<VerifiedScanInput> {
    let path = direct_child_path(scope_path, input_path)?;
    let (mut file, metadata) = open_verified_regular_file(&path)?;
    if metadata.len() > max_bytes {
        return Err(crate::PipelineError::contract(
            "KIO-E-SCAN-INPUT-OVERSIZED-001",
            format!(
                "input {} is {} bytes, above the {} byte limit",
                path.display(),
                metadata.len(),
                max_bytes
            ),
        ));
    }

    let initial_capacity = usize::try_from(metadata.len()).map_err(|_| {
        crate::PipelineError::contract(
            "KIO-E-SCAN-INPUT-OVERSIZED-001",
            format!("input {} cannot fit in process memory", path.display()),
        )
    })?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(initial_capacity).map_err(|_| {
        crate::PipelineError::contract(
            "KIO-E-SCAN-INPUT-OVERSIZED-001",
            format!("input {} cannot fit in process memory", path.display()),
        )
    })?;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let remaining = max_bytes.saturating_sub(bytes.len() as u64);
        let read_cap = std::cmp::min(buffer.len() as u64, remaining.saturating_add(1)) as usize;
        let read = file.read(&mut buffer[..read_cap]).pipeline_io(&path)?;
        if read == 0 {
            break;
        }
        if read as u64 > remaining {
            return Err(crate::PipelineError::contract(
                "KIO-E-SCAN-INPUT-OVERSIZED-001",
                format!(
                    "input {} grew beyond the {} byte limit",
                    path.display(),
                    max_bytes
                ),
            ));
        }
        bytes.try_reserve_exact(read).map_err(|_| {
            crate::PipelineError::contract(
                "KIO-E-SCAN-INPUT-OVERSIZED-001",
                format!("input {} cannot fit in process memory", path.display()),
            )
        })?;
        bytes.extend_from_slice(&buffer[..read]);
    }
    ensure_file_unchanged(&file, &metadata, &path)?;

    Ok(VerifiedScanInput {
        size_bytes: bytes.len() as u64,
        raw_hash: hash_bytes(&bytes),
        bytes,
    })
}

/// Hash a direct-child input with fixed working memory through the same verified
/// file handle used for metadata checks. The `max + 1` reader bounds growth while
/// hashing, so callers can scan for an identity without materializing file bytes.
pub fn hash_verified_scan_input(
    scope_path: &Path,
    input_path: &str,
    max_bytes: u64,
) -> Result<VerifiedScanIdentity> {
    let path = direct_child_path(scope_path, input_path)?;
    let (mut file, metadata) = open_verified_regular_file(&path)?;
    if metadata.len() > max_bytes {
        return Err(crate::PipelineError::contract(
            "KIO-E-SCAN-INPUT-OVERSIZED-001",
            format!(
                "input {} is {} bytes, above the {} byte limit",
                path.display(),
                metadata.len(),
                max_bytes
            ),
        ));
    }
    let raw_hash = {
        let mut limited = (&mut file).take(max_bytes.saturating_add(1));
        let raw_hash = hash_reader(&mut limited).pipeline_io(&path)?;
        if limited.limit() == 0 {
            return Err(crate::PipelineError::contract(
                "KIO-E-SCAN-INPUT-OVERSIZED-001",
                format!(
                    "input {} grew beyond the {} byte limit",
                    path.display(),
                    max_bytes
                ),
            ));
        }
        raw_hash
    };
    ensure_file_unchanged(&file, &metadata, &path)?;
    Ok(VerifiedScanIdentity {
        raw_hash,
        size_bytes: metadata.len(),
    })
}

/// Descriptor-bound counterpart to [`read_verified_scan_input`]. The input
/// name is validated as one direct child and opened without following a final
/// symlink through the retained root descriptor.
pub fn read_bound_verified_scan_input(
    root: &File,
    input_path: &str,
    max_bytes: u64,
) -> Result<VerifiedScanInput> {
    validate_direct_child_name(input_path)?;
    let path = Path::new(input_path);
    let (mut file, metadata) = open_bound_verified_regular_file(root, input_path)?;
    if metadata.len() > max_bytes {
        return Err(bound_input_oversized(input_path, metadata.len(), max_bytes));
    }
    let initial_capacity = usize::try_from(metadata.len()).map_err(|_| {
        crate::PipelineError::contract(
            "KIO-E-SCAN-INPUT-OVERSIZED-001",
            format!("input {input_path} cannot fit in process memory"),
        )
    })?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(initial_capacity).map_err(|_| {
        crate::PipelineError::contract(
            "KIO-E-SCAN-INPUT-OVERSIZED-001",
            format!("input {input_path} cannot fit in process memory"),
        )
    })?;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let remaining = max_bytes.saturating_sub(bytes.len() as u64);
        let read_cap = std::cmp::min(buffer.len() as u64, remaining.saturating_add(1)) as usize;
        let read = file.read(&mut buffer[..read_cap]).pipeline_io(path)?;
        if read == 0 {
            break;
        }
        if read as u64 > remaining {
            return Err(bound_input_oversized(
                input_path,
                max_bytes.saturating_add(1),
                max_bytes,
            ));
        }
        bytes.try_reserve_exact(read).map_err(|_| {
            crate::PipelineError::contract(
                "KIO-E-SCAN-INPUT-OVERSIZED-001",
                format!("input {input_path} cannot fit in process memory"),
            )
        })?;
        bytes.extend_from_slice(&buffer[..read]);
    }
    ensure_bound_file_unchanged(&file, &metadata, path)?;
    Ok(VerifiedScanInput {
        size_bytes: bytes.len() as u64,
        raw_hash: hash_bytes(&bytes),
        bytes,
    })
}

/// Descriptor-bound counterpart to [`hash_verified_scan_input`].
pub fn hash_bound_verified_scan_input(
    root: &File,
    input_path: &str,
    max_bytes: u64,
) -> Result<VerifiedScanIdentity> {
    validate_direct_child_name(input_path)?;
    let path = Path::new(input_path);
    let (mut file, metadata) = open_bound_verified_regular_file(root, input_path)?;
    if metadata.len() > max_bytes {
        return Err(bound_input_oversized(input_path, metadata.len(), max_bytes));
    }
    let mut limited = (&mut file).take(max_bytes.saturating_add(1));
    let raw_hash = hash_reader(&mut limited).pipeline_io(path)?;
    if limited.limit() == 0 {
        return Err(bound_input_oversized(
            input_path,
            max_bytes.saturating_add(1),
            max_bytes,
        ));
    }
    ensure_bound_file_unchanged(&file, &metadata, path)?;
    Ok(VerifiedScanIdentity {
        raw_hash,
        size_bytes: metadata.len(),
    })
}

/// Re-evaluate current ignore authorization and byte identity for a durable task.
/// Errors are intentionally distinct from `false` so callers can fail closed while
/// retaining an audit reason.
pub fn current_scan_allows_file(
    scope_path: &Path,
    input_path: &str,
    expected_raw_hash: &str,
    max_bytes: u64,
) -> Result<bool> {
    if !current_scan_policy_allows_file(scope_path, input_path)? {
        return Ok(false);
    }
    let identity = hash_verified_scan_input(scope_path, input_path, max_bytes)?;
    Ok(identity.raw_hash == expected_raw_hash)
}

/// Re-evaluate path classification without reopening input bytes. Callers that
/// already hold a verified buffer can bind its hash separately and use this check
/// immediately before a sink without creating a second pathname-read race.
pub fn current_scan_policy_allows_file(scope_path: &Path, input_path: &str) -> Result<bool> {
    let path = direct_child_path(scope_path, input_path)?;
    if input_path == ".kio" || input_path == ".kioignore" {
        return Ok(false);
    }
    if is_xdg_state_inside_scope(scope_path, &path) {
        return Ok(false);
    }
    let listed = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(crate::PipelineError::Io {
                path: path.display().to_string(),
                message: err.to_string(),
            });
        }
    };
    if !listed.file_type().is_file() {
        return Ok(false);
    }

    let case_insensitive = probe_case_insensitive(scope_path);
    let mut ignore_rules = load_config_ignore(scope_path)?;
    ignore_rules.extend(load_kioignore(scope_path)?);
    let secret = classify_secret(input_path);
    let ignored = try_ignored_by_rules(input_path, false, &ignore_rules, case_insensitive)?
        || secret == Some(SecretTier::TierA)
            && !try_explicitly_unignored(input_path, false, &ignore_rules, case_insensitive)?;
    if ignored {
        return Ok(false);
    }

    Ok(true)
}

/// Re-evaluate policy from retained directory handles without resolving a
/// public scope pathname. Ancestor denial is evaluated as a separate authority
/// group, so a target-local negation can never restore an inherited exclusion.
pub fn current_bound_scan_policy_allows_file(
    root: &File,
    kio: &File,
    input_path: &str,
    inherited_rules: &[IgnoreRule],
) -> Result<bool> {
    validate_direct_child_name(input_path)?;
    if input_path == ".kio" || input_path == ".kioignore" {
        return Ok(false);
    }
    let path = Path::new(input_path);
    let listed = match cap_fs::stat(root, path, cap_fs::FollowSymlinks::No) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(crate::PipelineError::Io {
                path: input_path.to_owned(),
                message: error.to_string(),
            });
        }
    };
    if !listed.file_type().is_file() {
        return Ok(false);
    }
    let case_insensitive = probe_bound_case_insensitive(kio);
    if rules_ignore_path(input_path, false, inherited_rules, case_insensitive)? {
        return Ok(false);
    }
    let mut local_rules = load_bound_config_ignore(kio)?;
    local_rules.extend(load_bound_kioignore(root)?);
    let secret = classify_secret(input_path);
    let ignored = try_ignored_by_rules(input_path, false, &local_rules, case_insensitive)?
        || secret == Some(SecretTier::TierA)
            && !try_explicitly_unignored(input_path, false, &local_rules, case_insensitive)?;
    Ok(!ignored)
}

fn direct_child_path(scope_path: &Path, input_path: &str) -> Result<PathBuf> {
    validate_direct_child_name(input_path)?;
    Ok(scope_path.join(input_path))
}

fn validate_direct_child_name(input_path: &str) -> Result<()> {
    let relative = Path::new(input_path);
    let mut components = relative.components();
    let valid = !input_path.is_empty()
        && !input_path.contains('/')
        && !input_path.contains('\\')
        && matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none();
    if !valid {
        return Err(crate::PipelineError::path(input_path));
    }
    Ok(())
}

fn bound_input_oversized(input_path: &str, size: u64, max_bytes: u64) -> crate::PipelineError {
    crate::PipelineError::contract(
        "KIO-E-SCAN-INPUT-OVERSIZED-001",
        format!("input {input_path} is {size} bytes, above the {max_bytes} byte limit"),
    )
}

fn open_bound_verified_regular_file(
    root: &File,
    input_path: &str,
) -> Result<(File, cap_fs::Metadata)> {
    validate_direct_child_name(input_path)?;
    let path = Path::new(input_path);
    let listed = cap_fs::stat(root, path, cap_fs::FollowSymlinks::No).pipeline_io(path)?;
    if !listed.file_type().is_file() {
        return Err(crate::PipelineError::contract(
            "KIO-E-SCAN-FILE-IDENTITY-001",
            format!("scan candidate is not a regular file: {input_path}"),
        ));
    }
    let mut options = cap_fs::OpenOptions::new();
    options.read(true);
    options._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let file = cap_fs::open(root, path, &options).pipeline_io(path)?;
    let opened = cap_fs::Metadata::from_file(&file).pipeline_io(path)?;
    let after = cap_fs::stat(root, path, cap_fs::FollowSymlinks::No).pipeline_io(path)?;
    if !opened.file_type().is_file()
        || !same_bound_file_identity(&listed, &opened)
        || !same_bound_file_identity(&opened, &after)
    {
        return Err(crate::PipelineError::contract(
            "KIO-E-SCAN-FILE-IDENTITY-001",
            format!("scan candidate changed while it was being opened: {input_path}"),
        ));
    }
    Ok((file, opened))
}

fn ensure_bound_file_unchanged(
    file: &File,
    before: &cap_fs::Metadata,
    display: &Path,
) -> Result<()> {
    let after = cap_fs::Metadata::from_file(file).pipeline_io(display)?;
    if !after.file_type().is_file() || !same_bound_file_state(before, &after) {
        return Err(crate::PipelineError::contract(
            "KIO-E-SCAN-INPUT-CHANGED-001",
            format!(
                "scan candidate changed while it was being read: {}",
                display.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn same_bound_file_identity(left: &cap_fs::Metadata, right: &cap_fs::Metadata) -> bool {
    use cap_fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(windows)]
fn same_bound_file_identity(left: &cap_fs::Metadata, right: &cap_fs::Metadata) -> bool {
    use cap_fs::_WindowsByHandle;
    left.volume_serial_number() == right.volume_serial_number()
        && left.file_index() == right.file_index()
}

#[cfg(not(any(unix, windows)))]
fn same_bound_file_identity(left: &cap_fs::Metadata, right: &cap_fs::Metadata) -> bool {
    left.len() == right.len() && left.modified().ok() == right.modified().ok()
}

#[cfg(unix)]
fn same_bound_file_state(left: &cap_fs::Metadata, right: &cap_fs::Metadata) -> bool {
    use cap_fs::MetadataExt;
    same_bound_file_identity(left, right)
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

#[cfg(not(unix))]
fn same_bound_file_state(left: &cap_fs::Metadata, right: &cap_fs::Metadata) -> bool {
    same_bound_file_identity(left, right)
}

#[derive(Debug)]
struct OpenedFileState {
    #[cfg(not(windows))]
    metadata: Metadata,
    #[cfg(windows)]
    information: crate::windows_file::WindowsFileInformation,
}

impl OpenedFileState {
    fn len(&self) -> u64 {
        #[cfg(windows)]
        {
            self.information.file_size()
        }
        #[cfg(not(windows))]
        {
            self.metadata.len()
        }
    }
}

fn open_verified_regular_file(path: &Path) -> Result<(File, OpenedFileState)> {
    #[cfg(windows)]
    {
        let listed = crate::windows_file::open_path_no_follow(path).pipeline_io(path)?;
        let listed_information = crate::windows_file::information(&listed).pipeline_io(path)?;
        if !listed_information.is_regular_file() {
            return Err(crate::PipelineError::contract(
                "KIO-E-SCAN-FILE-IDENTITY-001",
                format!("scan candidate is not a regular file: {}", path.display()),
            ));
        }
        let file = File::open(path).pipeline_io(path)?;
        verify_opened_regular_file(path, listed_information, file)
    }

    #[cfg(not(windows))]
    {
        // `symlink_metadata` does not follow the final component. Comparing its identity
        // with the opened handle closes the swap-to-symlink interval without requiring a
        // later pathname read.
        let listed = std::fs::symlink_metadata(path).pipeline_io(path)?;
        if !listed.file_type().is_file() {
            return Err(crate::PipelineError::contract(
                "KIO-E-SCAN-FILE-IDENTITY-001",
                format!("scan candidate is not a regular file: {}", path.display()),
            ));
        }
        let file = File::open(path).pipeline_io(path)?;
        verify_opened_regular_file(path, listed, file)
    }
}

#[cfg(not(windows))]
fn verify_opened_regular_file(
    path: &Path,
    listed: Metadata,
    file: File,
) -> Result<(File, OpenedFileState)> {
    let opened = file.metadata().pipeline_io(path)?;
    if !opened.is_file() || !same_file_identity(&listed, &opened) {
        return Err(crate::PipelineError::contract(
            "KIO-E-SCAN-FILE-IDENTITY-001",
            format!(
                "scan candidate changed while it was being opened: {}",
                path.display()
            ),
        ));
    }
    Ok((file, OpenedFileState { metadata: opened }))
}

#[cfg(windows)]
fn verify_opened_regular_file(
    path: &Path,
    listed: crate::windows_file::WindowsFileInformation,
    file: File,
) -> Result<(File, OpenedFileState)> {
    let opened = file.metadata().pipeline_io(path)?;
    let information = crate::windows_file::information(&file).pipeline_io(path)?;
    if !opened.is_file() || !information.is_regular_file() || !listed.same_identity(information) {
        return Err(crate::PipelineError::contract(
            "KIO-E-SCAN-FILE-IDENTITY-001",
            format!(
                "scan candidate changed while it was being opened: {}",
                path.display()
            ),
        ));
    }
    Ok((file, OpenedFileState { information }))
}

fn ensure_file_unchanged(file: &File, before: &OpenedFileState, path: &Path) -> Result<()> {
    #[cfg(windows)]
    let unchanged = {
        let after = crate::windows_file::information(file).pipeline_io(path)?;
        after.is_regular_file() && before.information.same_file_state(after)
    };
    #[cfg(not(windows))]
    let unchanged = {
        let after = file.metadata().pipeline_io(path)?;
        same_file_state(&before.metadata, &after)
    };
    if !unchanged {
        return Err(crate::PipelineError::contract(
            "KIO-E-SCAN-INPUT-CHANGED-001",
            format!(
                "scan candidate changed while it was being read: {}",
                path.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn same_file_identity(left: &Metadata, right: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(any(unix, windows)))]
fn same_file_identity(left: &Metadata, right: &Metadata) -> bool {
    left.file_type() == right.file_type()
        && left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
}

#[cfg(unix)]
fn same_file_state(left: &Metadata, right: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    same_file_identity(left, right)
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

#[cfg(not(any(unix, windows)))]
fn same_file_state(left: &Metadata, right: &Metadata) -> bool {
    same_file_identity(left, right)
}

fn is_xdg_state_inside_scope(scope_path: &Path, path: &Path) -> bool {
    let scope_path = scope_path
        .canonicalize()
        .unwrap_or_else(|_| scope_path.to_path_buf());
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    [
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "XDG_STATE_HOME",
        "XDG_RUNTIME_DIR",
    ]
    .iter()
    .filter_map(std::env::var_os)
    .map(PathBuf::from)
    .map(|path| path.canonicalize().unwrap_or(path))
    .filter(|xdg| xdg.starts_with(&scope_path))
    .any(|xdg| path == xdg || path.starts_with(&xdg))
}

pub fn load_kioignore(scope_path: &Path) -> Result<Vec<IgnoreRule>> {
    let path = scope_path.join(".kioignore");
    let Some(content) = read_optional_bounded_local_text(&path)? else {
        return Ok(Vec::new());
    };
    parse_kioignore_text(&content)
}

pub fn load_config_ignore(scope_path: &Path) -> Result<Vec<IgnoreRule>> {
    let mut rules = load_generated_parent_ignore(scope_path)?;
    rules.extend(load_local_config_ignore(scope_path)?);
    Ok(rules)
}

fn load_generated_parent_ignore(scope_path: &Path) -> Result<Vec<IgnoreRule>> {
    let path = scope_path.join(".kio/config.toml");
    let Some(text) = read_optional_bounded_local_text(&path)? else {
        return Ok(Vec::new());
    };
    let value: toml::Value =
        toml::from_str(&text).map_err(|err| crate::PipelineError::Schema(err.to_string()))?;
    let generated = match value.get("generated_parent_policy") {
        Some(policy) => {
            let policy: GeneratedParentPolicy = policy.clone().try_into().map_err(|err| {
                crate::PipelineError::Schema(format!("invalid generated_parent_policy: {err}"))
            })?;
            if policy.rules.len() > MAX_GENERATED_PARENT_IGNORE_RULES {
                return Err(crate::PipelineError::Schema(
                    "generated parent ignore policy exceeds rule cap".to_owned(),
                ));
            }
            for rule in &policy.rules {
                if rule.scope_prefix.is_none() {
                    return Err(crate::PipelineError::Schema(
                        "generated parent ignore rule requires scope_prefix".to_owned(),
                    ));
                }
                validate_ignore_rule(rule)?;
            }
            policy.rules
        }
        None => Vec::new(),
    };
    Ok(generated)
}

fn load_local_config_ignore(scope_path: &Path) -> Result<Vec<IgnoreRule>> {
    let path = scope_path.join(".kio/config.toml");
    let Some(text) = read_optional_bounded_local_text(&path)? else {
        return Ok(Vec::new());
    };
    parse_local_config_ignore_text(&text)
}

/// Parse only the local `[scope].ignore` authority.  In particular this never
/// consults the deprecated generated-parent-policy field: callers which need
/// a live effective policy must read every validated ancestor themselves.
pub(crate) fn parse_local_config_ignore_text(text: &str) -> Result<Vec<IgnoreRule>> {
    let value: toml::Value =
        toml::from_str(text).map_err(|err| crate::PipelineError::Schema(err.to_string()))?;
    let Some(ignore) = value
        .get("scope")
        .and_then(|scope| scope.get("ignore"))
        .and_then(toml::Value::as_array)
    else {
        return Ok(Vec::new());
    };
    let local = ignore
        .iter()
        .map(|value| {
            let pattern = value.as_str().ok_or_else(|| {
                crate::PipelineError::Schema("scope.ignore entries must be strings".to_owned())
            })?;
            let rule = IgnoreRule {
                pattern: pattern.trim_start_matches('!').to_owned(),
                negated: pattern.starts_with('!'),
                scope_prefix: None,
            };
            validate_ignore_rule(&rule)?;
            Ok(rule)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(local)
}

fn read_optional_bounded_local_text(path: &Path) -> Result<Option<String>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() || metadata.file_type().is_symlink() => {
            Err(crate::PipelineError::contract(
                "KIO-E-SCAN-FILE-IDENTITY-001",
                format!(
                    "scan configuration is not a bounded regular file: {}",
                    path.display()
                ),
            ))
        }
        Ok(_) => {
            let bytes =
                kio_core::cas::read_bounded_regular_file(path, MAX_BOUND_SCAN_METADATA_BYTES)
                    .map_err(|error| {
                        crate::PipelineError::contract(
                            "KIO-E-SCAN-FILE-IDENTITY-001",
                            format!(
                                "scan configuration is absent, unsafe, or changed: {}: {error}",
                                path.display()
                            ),
                        )
                    })?;
            let text = String::from_utf8(bytes)
                .map_err(|error| crate::PipelineError::Schema(error.to_string()))?;
            Ok(Some(text))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(crate::PipelineError::Io {
            path: path.display().to_string(),
            message: error.to_string(),
        }),
    }
}

/// Read the strict child policy from the retained `.kio` directory. Generated
/// ancestor policy stays first, so a child-local negation has the same
/// precedence as it would in an ordinary public-path scan.
fn load_bound_config_ignore(kio: &File) -> Result<Vec<IgnoreRule>> {
    let Some(text) = read_bound_optional_regular_text(kio, "config.toml")? else {
        return Ok(Vec::new());
    };
    let value: toml::Value =
        toml::from_str(&text).map_err(|error| crate::PipelineError::Schema(error.to_string()))?;
    let mut rules = match value.get("generated_parent_policy") {
        Some(policy) => {
            let policy: GeneratedParentPolicy = policy.clone().try_into().map_err(|error| {
                crate::PipelineError::Schema(format!("invalid generated_parent_policy: {error}"))
            })?;
            if policy.rules.len() > MAX_GENERATED_PARENT_IGNORE_RULES {
                return Err(crate::PipelineError::Schema(
                    "generated parent ignore policy exceeds rule cap".to_owned(),
                ));
            }
            for rule in &policy.rules {
                if rule.scope_prefix.is_none() {
                    return Err(crate::PipelineError::Schema(
                        "generated parent ignore rule requires scope_prefix".to_owned(),
                    ));
                }
                validate_ignore_rule(rule)?;
            }
            policy.rules
        }
        None => Vec::new(),
    };
    let Some(local) = value
        .get("scope")
        .and_then(|scope| scope.get("ignore"))
        .and_then(toml::Value::as_array)
    else {
        return Ok(rules);
    };
    for item in local {
        let pattern = item.as_str().ok_or_else(|| {
            crate::PipelineError::Schema("scope.ignore entries must be strings".to_owned())
        })?;
        let rule = IgnoreRule {
            pattern: pattern.trim_start_matches('!').to_owned(),
            negated: pattern.starts_with('!'),
            scope_prefix: None,
        };
        validate_ignore_rule(&rule)?;
        rules.push(rule);
    }
    Ok(rules)
}

fn load_bound_kioignore(root: &File) -> Result<Vec<IgnoreRule>> {
    let Some(content) = read_bound_optional_regular_text(root, ".kioignore")? else {
        return Ok(Vec::new());
    };
    parse_kioignore_text(&content)
}

/// Parse the direct `.kioignore` file with the shared bounded rule grammar.
pub(crate) fn parse_kioignore_text(content: &str) -> Result<Vec<IgnoreRule>> {
    let rules = content
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                return None;
            }
            let (negated, pattern) = trimmed
                .strip_prefix('!')
                .map(|pattern| (true, pattern))
                .unwrap_or((false, trimmed));
            Some(IgnoreRule {
                pattern: pattern.to_owned(),
                negated,
                scope_prefix: None,
            })
        })
        .map(|rule| {
            validate_ignore_rule(&rule)?;
            Ok(rule)
        })
        .collect::<Result<Vec<_>>>()?;
    if rules.len() > MAX_GENERATED_PARENT_IGNORE_RULES {
        return Err(crate::PipelineError::Schema(
            "ignore file exceeds rule cap".to_owned(),
        ));
    }
    Ok(rules)
}

fn read_bound_optional_regular_text(dir: &File, name: &str) -> Result<Option<String>> {
    validate_direct_child_name(name)?;
    let path = Path::new(name);
    let listed = match cap_fs::stat(dir, path, cap_fs::FollowSymlinks::No) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(crate::PipelineError::Io {
                path: name.to_owned(),
                message: error.to_string(),
            });
        }
    };
    if !listed.file_type().is_file() || listed.len() > MAX_BOUND_SCAN_METADATA_BYTES {
        return Err(crate::PipelineError::contract(
            "KIO-E-SCAN-FILE-IDENTITY-001",
            format!("scan configuration is not a bounded regular file: {name}"),
        ));
    }
    let mut options = cap_fs::OpenOptions::new();
    options.read(true);
    options._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let mut file = cap_fs::open(dir, path, &options).pipeline_io(path)?;
    let opened = cap_fs::Metadata::from_file(&file).pipeline_io(path)?;
    let after = cap_fs::stat(dir, path, cap_fs::FollowSymlinks::No).pipeline_io(path)?;
    if !opened.file_type().is_file()
        || opened.len() > MAX_BOUND_SCAN_METADATA_BYTES
        || !same_bound_file_identity(&listed, &opened)
        || !same_bound_file_identity(&opened, &after)
    {
        return Err(crate::PipelineError::contract(
            "KIO-E-SCAN-FILE-IDENTITY-001",
            format!("scan configuration changed while it was being opened: {name}"),
        ));
    }
    let capacity = usize::try_from(opened.len()).map_err(|_| {
        crate::PipelineError::contract(
            "KIO-E-SCAN-INPUT-OVERSIZED-001",
            format!("scan configuration cannot fit in process memory: {name}"),
        )
    })?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(capacity).map_err(|_| {
        crate::PipelineError::contract(
            "KIO-E-SCAN-INPUT-OVERSIZED-001",
            format!("scan configuration cannot fit in process memory: {name}"),
        )
    })?;
    // Bound the descriptor read as well as the pre-open observation. A writer
    // can extend a regular file after metadata was checked; `read_to_string`
    // would then grow until EOF before the later identity check could reject.
    let mut limited = (&mut file).take(opened.len().saturating_add(1));
    limited.read_to_end(&mut bytes).pipeline_io(path)?;
    if bytes.len() as u64 != opened.len() {
        return Err(crate::PipelineError::contract(
            "KIO-E-SCAN-INPUT-CHANGED-001",
            format!("scan configuration changed while it was being read: {name}"),
        ));
    }
    ensure_bound_file_unchanged(&file, &opened, path)?;
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|error| crate::PipelineError::Schema(error.to_string()))
}

/// QA7 (step4b-contract-tests-p3a.md §B): the Tier B needle set, exposed so
/// `kio_core::scope::tier_a_template_text`'s `effective_ignore_hash` input
/// (10 §1.1) can include it — kept in this one place so [`classify_secret`]
/// and the hash template can never drift.
pub const TIER_B_NEEDLES: &[&str] = &["credentials", "secret", "token", "apikey", "password"];

#[must_use]
pub fn classify_secret(path: &str) -> Option<SecretTier> {
    let normalized = path.trim_start_matches('/').replace('\\', "/");
    let name = normalized.rsplit('/').next().unwrap_or(&normalized);
    let lower = name.to_ascii_lowercase();
    if kio_core::scope::is_tier_a_secret_name(path) {
        return Some(SecretTier::TierA);
    }
    let tier_b = TIER_B_NEEDLES.iter().any(|needle| lower.contains(needle));
    tier_b.then_some(SecretTier::TierB)
}

/// R10-3: probe whether `scope_path`'s volume treats names case-insensitively
/// (APFS default, exFAT, NTFS) — the moral equivalent of git's `core.ignorecase`.
/// Writes a unique probe file and checks whether a name differing only in case
/// resolves to it. Best-effort: any I/O failure returns `false` (assume
/// case-sensitive) so two genuinely distinct names on a case-sensitive volume are
/// never folded together (which would be its own silent data loss). The probe
/// file(s) are always removed.
fn probe_case_insensitive(scope_path: &Path) -> bool {
    // Probe inside `.kio` (present for an initialized scope and itself skipped by the
    // scanner) so the probe file never appears among the user's candidates; fall back
    // to the scope root if `.kio` is somehow absent.
    let dir = {
        let kio = scope_path.join(".kio");
        if kio.is_dir() {
            kio
        } else {
            scope_path.to_path_buf()
        }
    };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let stem = format!(".kio-caseprobe-{}-{}", std::process::id(), nanos);
    let lower = dir.join(format!("{stem}-a"));
    let upper = dir.join(format!("{stem}-A"));
    if std::fs::write(&lower, b"").is_err() {
        return false;
    }
    let insensitive = upper.exists();
    // On an insensitive FS `lower` and `upper` are the same inode, so removing `lower`
    // clears both; on a sensitive FS `upper` was never created.
    let _ = std::fs::remove_file(&lower);
    let _ = std::fs::remove_file(&upper);
    insensitive
}

/// Descriptor-relative equivalent of [`probe_case_insensitive`]. The probe
/// lives in the retained `.kio` handle and never becomes a source candidate.
/// Probe failure is conservative: preserve distinct names by assuming a
/// case-sensitive volume.
fn probe_bound_case_insensitive(kio: &File) -> bool {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let stem = format!(".kio-caseprobe-{}-{nanos}", std::process::id());
    let lower = format!("{stem}-a");
    let upper = format!("{stem}-A");
    let mut options = cap_fs::OpenOptions::new();
    options.write(true).create_new(true);
    if cap_fs::open(kio, Path::new(&lower), &options).is_err() {
        return false;
    }
    let insensitive = cap_fs::stat(kio, Path::new(&upper), cap_fs::FollowSymlinks::No).is_ok();
    let _ = cap_fs::remove_file(kio, Path::new(&lower));
    let _ = cap_fs::remove_file(kio, Path::new(&upper));
    insensitive
}

pub(crate) fn rules_ignore_path(
    path: &str,
    is_dir: bool,
    rules: &[IgnoreRule],
    case_insensitive: bool,
) -> Result<bool> {
    let mut ignored = false;
    for rule in rules {
        let effective_path = apply_scope_prefix(path, rule)?;
        if matches_ignore_pattern(&effective_path, is_dir, &rule.pattern, case_insensitive)? {
            ignored = !rule.negated;
        }
    }
    Ok(ignored)
}

pub(crate) fn rules_explicitly_unignore_path(
    path: &str,
    is_dir: bool,
    rules: &[IgnoreRule],
    case_insensitive: bool,
) -> Result<bool> {
    for rule in rules.iter().filter(|rule| rule.negated) {
        let effective_path = apply_scope_prefix(path, rule)?;
        if matches_ignore_pattern(&effective_path, is_dir, &rule.pattern, case_insensitive)? {
            return Ok(true);
        }
    }
    Ok(false)
}

// Existing scan call sites use these private spellings. Keep their behavior
// routed through the policy helpers so the matcher has one implementation.
fn try_ignored_by_rules(
    path: &str,
    is_dir: bool,
    rules: &[IgnoreRule],
    case_insensitive: bool,
) -> Result<bool> {
    rules_ignore_path(path, is_dir, rules, case_insensitive)
}

fn try_explicitly_unignored(
    path: &str,
    is_dir: bool,
    rules: &[IgnoreRule],
    case_insensitive: bool,
) -> Result<bool> {
    rules_explicitly_unignore_path(path, is_dir, rules, case_insensitive)
}

fn apply_scope_prefix(path: &str, rule: &IgnoreRule) -> Result<String> {
    validate_ignore_rule(rule)?;
    Ok(match &rule.scope_prefix {
        Some(prefix) if !prefix.is_empty() => format!("{prefix}/{path}"),
        _ => path.to_owned(),
    })
}

fn validate_ignore_rule(rule: &IgnoreRule) -> Result<()> {
    if rule.pattern.is_empty() || rule.pattern.len() > MAX_GENERATED_PARENT_IGNORE_PATTERN_BYTES {
        return Err(crate::PipelineError::Schema(
            "ignore pattern is empty or exceeds the policy limit".to_owned(),
        ));
    }
    if let Some(prefix) = &rule.scope_prefix {
        if prefix.len() > MAX_GENERATED_PARENT_IGNORE_PREFIX_BYTES {
            return Err(crate::PipelineError::Schema(
                "generated parent ignore prefix exceeds the policy limit".to_owned(),
            ));
        }
        validate_scope_prefix(prefix)?;
    }
    Ok(())
}

fn validate_scope_prefix(prefix: &str) -> Result<()> {
    if prefix.is_empty() {
        return Ok(());
    }
    if prefix.contains('\\') || prefix.contains('\0') || Path::new(prefix).is_absolute() {
        return Err(crate::PipelineError::Schema(
            "generated parent ignore prefix must be a relative slash path".to_owned(),
        ));
    }
    if Path::new(prefix)
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(crate::PipelineError::Schema(
            "generated parent ignore prefix contains a non-normal component".to_owned(),
        ));
    }
    Ok(())
}

fn join_scope_prefix(existing: Option<&str>, child: &str) -> String {
    match existing.filter(|prefix| !prefix.is_empty()) {
        Some(prefix) => format!("{prefix}/{child}"),
        None => child.to_owned(),
    }
}

fn matches_ignore_pattern(
    path: &str,
    is_dir: bool,
    pattern: &str,
    case_insensitive: bool,
) -> Result<bool> {
    // R9-1: match on the NFC projection of BOTH sides so a Unicode canonically
    // equivalent `.kioignore` / `[scope] ignore` pattern reliably excludes a file
    // whose on-disk name uses a different normal form (NFD names are routine on
    // macOS/APFS via Finder/iCloud/zip/IME). This normalizes only the *matching*
    // projection — `ScanCandidate.input_path` and the CAS/identity raw bytes stay
    // the original bytes (R8 F2: normalize the comparison, never the identity).
    let mut path = path.nfc().collect::<String>();
    let mut pattern = pattern.nfc().collect::<String>();
    // R10-3: on a case-insensitive volume, fold BOTH sides to lowercase (Unicode
    // aware) after NFC so a pattern whose case differs from the on-disk name still
    // excludes it — matching git's `core.ignorecase` behavior. The glob metacharacters
    // (`*`, `?`, `/`, `**`) are ASCII and unaffected by lowercasing. On a
    // case-sensitive volume we do NOT fold (folding would wrongly exclude a distinct
    // file, another silent data loss).
    if case_insensitive {
        path = path.to_lowercase();
        pattern = pattern.to_lowercase();
    }
    let directory_only = pattern.ends_with('/');
    if directory_only && !is_dir {
        return Ok(false);
    }
    let rooted = pattern.starts_with('/');
    let pattern = pattern.trim_start_matches('/').trim_end_matches('/');
    let normalized_path = path.trim_start_matches('/').replace('\\', "/");
    if !rooted && !pattern.contains('/') {
        return wildcard_match(
            pattern,
            normalized_path
                .rsplit('/')
                .next()
                .unwrap_or(&normalized_path),
        );
    }
    wildcard_match(pattern, &normalized_path)
}

fn wildcard_match(pattern: &str, value: &str) -> Result<bool> {
    // Match Unicode scalar values: `?` consumes one scalar, never one UTF-8 byte.
    // The bottom-up state table visits each (pattern, value) pair at most once,
    // replacing recursive backtracking with explicitly bounded work.
    let pattern = pattern.chars().collect::<Vec<_>>();
    let value = value.chars().collect::<Vec<_>>();
    let width = value.len().checked_add(1).ok_or_else(glob_budget_error)?;
    let states = pattern
        .len()
        .checked_add(1)
        .and_then(|height| height.checked_mul(width))
        .filter(|states| *states <= MAX_GLOB_STATES)
        .ok_or_else(glob_budget_error)?;
    let mut matched = vec![false; states];
    let at = |pi: usize, vi: usize| pi * width + vi;
    let mut next_slash = vec![None; width];
    let mut nearest = None;
    for vi in (0..value.len()).rev() {
        if value[vi] == '/' {
            nearest = Some(vi);
        }
        next_slash[vi] = nearest;
    }
    matched[at(pattern.len(), value.len())] = true;

    for pi in (0..pattern.len()).rev() {
        for vi in (0..=value.len()).rev() {
            let is_terminal_double_star =
                pi + 2 == pattern.len() && pattern[pi] == '*' && pattern[pi + 1] == '*';
            let is_double_star_directory = pi + 2 < pattern.len()
                && pattern[pi] == '*'
                && pattern[pi + 1] == '*'
                && pattern[pi + 2] == '/';
            matched[at(pi, vi)] = if is_terminal_double_star {
                true
            } else if is_double_star_directory {
                matched[at(pi + 3, vi)]
                    || next_slash[vi].is_some_and(|slash| matched[at(pi, slash + 1)])
            } else {
                match pattern[pi] {
                    '*' => {
                        matched[at(pi + 1, vi)]
                            || vi < value.len() && value[vi] != '/' && matched[at(pi, vi + 1)]
                    }
                    '?' => vi < value.len() && value[vi] != '/' && matched[at(pi + 1, vi + 1)],
                    ch => vi < value.len() && ch == value[vi] && matched[at(pi + 1, vi + 1)],
                }
            };
        }
    }
    Ok(matched[0])
}

fn glob_budget_error() -> crate::PipelineError {
    crate::PipelineError::contract(
        "KIO-E-SCAN-IGNORE-BUDGET-001",
        format!("ignore pattern exceeds the {MAX_GLOB_STATES} state matching budget"),
    )
}

fn media_type_for_path(path: &Path) -> &'static str {
    // R21-4: lowercase the extension so an uppercase-extension text-native file
    // (`README.MD`, `NOTE.TXT`, `MAIN.RS`) is recognized as text/markdown/plain/code and
    // handled locally — not folded to octet-stream and shipped to online OCR (R9-2).
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "md" | "markdown" => "text/markdown",
        "txt" => "text/plain",
        "rs" | "py" | "js" | "ts" | "go" | "java" | "c" | "h" | "cpp" => "text/x-code",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        // R20-6: recognize OOXML office documents by their real MIME so they are treated as
        // non-text-native (routed to online OCR), not folded into octet-stream and given a
        // raw-bytes local passthrough that evidences the ZIP bytes as searchable text.
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => "application/octet-stream",
    }
}

fn scope_id_from_scope_json(scope_path: &Path) -> Option<String> {
    let value = read_optional_bounded_local_text(&scope_path.join(".kio/scope.json")).ok()??;
    let value = serde_json::from_str::<serde_json::Value>(&value).ok()?;
    value.get("scope_id")?.as_str().map(str::to_owned)
}

fn bound_scope_id_from_scope_json(kio: &File) -> Option<String> {
    let value = read_bound_optional_regular_text(kio, "scope.json").ok()??;
    let value = serde_json::from_str::<serde_json::Value>(&value).ok()?;
    value.get("scope_id")?.as_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn bound_scan_uses_retained_handles_after_public_scope_replacement() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let public = temp.path().join("scope");
        let retained = temp.path().join("retained");
        let replacement = temp.path().join("replacement");
        std::fs::create_dir_all(public.join(".kio")).unwrap();
        std::fs::write(public.join("original.txt"), b"original").unwrap();
        std::fs::write(public.join(".kio/scope.json"), r#"{"scope_id":"original"}"#).unwrap();
        std::fs::write(
            public.join(".kio/config.toml"),
            "[generated_parent_policy]\nrules = []\n",
        )
        .unwrap();
        let root = File::open(&public).unwrap();
        let kio = File::open(public.join(".kio")).unwrap();

        std::fs::rename(&public, &retained).unwrap();
        std::fs::create_dir_all(&replacement).unwrap();
        std::fs::write(replacement.join("replacement.txt"), b"replacement").unwrap();
        symlink(&replacement, &public).unwrap();

        let preview = build_bound_scan_preview(
            &root,
            &kio,
            ScanPreviewRequest {
                scope_path: public.display().to_string(),
                include_raw_hashes: true,
                require_network_approval: false,
            },
            &[],
        )
        .unwrap();
        assert_eq!(preview.scope_id, "original");
        assert_eq!(preview.candidates.len(), 1);
        assert_eq!(preview.candidates[0].input_path, "original.txt");
        assert_eq!(
            preview.candidates[0].raw_hash,
            Some(hash_bytes(b"original"))
        );
        assert_eq!(
            read_bound_verified_scan_input(&root, "original.txt", 1024)
                .unwrap()
                .bytes,
            b"original"
        );
    }

    #[test]
    fn placeholder_scan_candidate_serializes() {
        let candidate = ScanCandidate {
            input_path: "report.pdf".to_owned(),
            media_type: "application/pdf".to_owned(),
            size_bytes: 42,
            raw_hash: None,
            ignored: false,
            quarantine_reason: None,
        };

        let value = serde_json::to_value(candidate).expect("serialize scan candidate");
        assert_eq!(value["input_path"], "report.pdf");
    }

    #[test]
    fn child_scope_vcs_boolean_must_be_strict() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".kio")).unwrap();
        std::fs::write(
            dir.path().join(".kio/config.toml"),
            "[scope]\nindex_vcs_repos = \"yes\"\n",
        )
        .unwrap();
        assert!(load_index_vcs_repos(dir.path()).is_err());
    }

    #[test]
    fn child_scope_discovery_reports_its_directory_entry_bound() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".kio")).unwrap();
        for index in 0..=MAX_CHILD_SCOPE_DIRECTORIES {
            std::fs::create_dir(dir.path().join(format!("child-{index:04}"))).unwrap();
        }

        let plan = discover_child_scopes(dir.path()).unwrap();
        assert_eq!(plan.candidates.len(), 1);
        assert_eq!(plan.candidates[0].path, "");
        assert_eq!(plan.candidates[0].status, "skipped_limit");
        assert_eq!(
            plan.candidates[0].reason.as_deref(),
            Some("directory_entry_cap")
        );
    }

    #[test]
    fn child_scope_discovery_keeps_large_typed_inherited_rules_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".kio")).unwrap();
        std::fs::create_dir_all(dir.path().join("child")).unwrap();
        std::fs::write(dir.path().join("child/note.md"), "body").unwrap();
        let rules = (0..40)
            .map(|index| format!("rule-{index:02}-{}", "x".repeat(2_000)))
            .collect::<Vec<_>>();
        let quoted = rules
            .iter()
            .map(|rule| format!("\"{rule}\""))
            .collect::<Vec<_>>()
            .join(", ");
        std::fs::write(
            dir.path().join(".kio/config.toml"),
            format!("[scope]\nignore = [{quoted}]\n"),
        )
        .unwrap();

        let plan = discover_child_scopes(dir.path()).unwrap();
        let child = bind_planned_child(&plan, "child").unwrap().unwrap();
        assert_eq!(child.canonical_root, dir.path().join("child"));
        assert_eq!(child.inherited_rules.len(), rules.len());
    }

    #[test]
    fn bound_child_rechecks_vcs_exclusion_after_discovery() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".kio")).unwrap();
        std::fs::create_dir_all(dir.path().join("child")).unwrap();
        std::fs::write(dir.path().join("child/note.md"), "body").unwrap();
        let plan = discover_child_scopes(dir.path()).unwrap();
        std::fs::write(dir.path().join("child/.git"), "gitdir: elsewhere\n").unwrap();
        assert!(bind_planned_child(&plan, "child").unwrap().is_none());
    }

    #[test]
    fn vcs_scope_root_prunes_all_descendant_child_scopes_by_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".kio")).unwrap();
        std::fs::write(dir.path().join(".git"), "gitdir: elsewhere\n").unwrap();
        std::fs::create_dir(dir.path().join("child")).unwrap();
        std::fs::write(dir.path().join("child/note.md"), "body").unwrap();

        let plan = discover_child_scopes(dir.path()).unwrap();
        assert_eq!(plan.candidates.len(), 1);
        assert_eq!(plan.candidates[0].path, "");
        assert_eq!(plan.candidates[0].status, "skipped_vcs");
        assert_eq!(
            plan.candidates[0].reason.as_deref(),
            Some("scope_root_is_vcs")
        );
    }

    // ------------------------------------------------------------------
    // QA19 (step4b-contract-tests-p3a.md §F): preview cost estimate wired to
    // tools.toml's declared [pricing].
    // ------------------------------------------------------------------

    fn candidate(media_type: &str, size_bytes: u64, ignored: bool) -> ScanCandidate {
        ScanCandidate {
            input_path: format!("f.{media_type}"),
            media_type: media_type.to_owned(),
            size_bytes,
            raw_hash: None,
            ignored,
            quarantine_reason: None,
        }
    }

    /// QA19: no declared pricing for either role -> both estimates are `0.0`
    /// (an honest "unknown", the same posture `write_approval_record` had
    /// before this fix — never a fabricated non-zero figure).
    #[test]
    fn qa19_no_pricing_declared_yields_zero_estimates() {
        let candidates = vec![candidate("application/pdf", 300_000, false)];
        let (markdownize_usd, embedding_usd) =
            estimated_enrichment_cost_usd(&candidates, &BTreeMap::new(), &BTreeMap::new());
        assert_eq!(markdownize_usd, 0.0);
        assert_eq!(embedding_usd, 0.0);
    }

    /// QA19: a declared markdownize `pages` price is multiplied by the
    /// estimated page count of non-text-native candidates only — a
    /// text-native candidate (skips OCR entirely, docs/04 §2) contributes
    /// bytes to the embedding estimate but NOT the markdownize one.
    #[test]
    fn qa19_markdownize_estimate_excludes_text_native_candidates() {
        let mut markdownize_pricing = BTreeMap::new();
        markdownize_pricing.insert("pages".to_owned(), 0.004);
        let candidates = vec![
            // 6_000 bytes / 3_000 bytes-per-page ~= 2 pages -> 2 * 0.004.
            candidate("application/pdf", 6_000, false),
            // Text-native: excluded from the markdownize byte sum.
            candidate("text/plain", 1_000_000, false),
            // Ignored: excluded from both sums.
            candidate("application/pdf", 1_000_000, true),
        ];
        let (markdownize_usd, _embedding_usd) =
            estimated_enrichment_cost_usd(&candidates, &markdownize_pricing, &BTreeMap::new());
        assert!(
            (markdownize_usd - 0.008).abs() < 1e-9,
            "got {markdownize_usd}"
        );
    }

    /// QA19: a declared embedding `tokens_in` price is multiplied by the
    /// estimated token count of ALL included candidates (text-native
    /// candidates DO get embedded, unlike markdownize).
    #[test]
    fn qa19_embedding_estimate_includes_text_native_candidates() {
        let mut embedding_pricing = BTreeMap::new();
        embedding_pricing.insert("tokens_in".to_owned(), 0.00000015);
        let candidates = vec![
            candidate("text/markdown", 4_000, false),   // 1_000 tokens
            candidate("application/pdf", 4_000, false), // 1_000 tokens
        ];
        let (_markdownize_usd, embedding_usd) =
            estimated_enrichment_cost_usd(&candidates, &BTreeMap::new(), &embedding_pricing);
        assert!(
            (embedding_usd - 2_000.0 * 0.00000015).abs() < 1e-12,
            "got {embedding_usd}"
        );
    }

    /// QA19: `build_scan_preview`'s `CostPreview.estimated_usd` is the sum of
    /// the two split fields (a regression-lock on the combined figure's
    /// definition, for any existing display call site that only reads it).
    #[test]
    fn qa19_combined_estimated_usd_is_the_sum_of_the_split_fields() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.pdf"), vec![0_u8; 100]).unwrap();
        let preview = build_scan_preview(ScanPreviewRequest {
            scope_path: dir.path().display().to_string(),
            include_raw_hashes: false,
            require_network_approval: false,
        })
        .unwrap();
        let cost = preview.estimated_cost.expect("cost preview present");
        assert!(
            (cost.estimated_usd - (cost.estimated_markdownize_usd + cost.estimated_embedding_usd))
                .abs()
                < 1e-9
        );
    }

    #[test]
    fn secrets_and_ignore_rules_are_applied_in_order() {
        let rules = vec![
            IgnoreRule {
                pattern: "*.log".to_owned(),
                negated: false,
                scope_prefix: None,
            },
            IgnoreRule {
                pattern: "keep.log".to_owned(),
                negated: true,
                scope_prefix: None,
            },
        ];
        assert_eq!(classify_secret(".env"), Some(SecretTier::TierA));
        assert_eq!(classify_secret("api_token.txt"), Some(SecretTier::TierB));
        assert!(try_ignored_by_rules("debug.log", false, &rules, false).unwrap());
        assert!(!try_ignored_by_rules("keep.log", false, &rules, false).unwrap());
    }

    #[test]
    fn ignore_rule_evaluation_reports_malformed_and_over_budget_rules() {
        let malformed = [IgnoreRule {
            pattern: String::new(),
            negated: false,
            scope_prefix: None,
        }];
        let malformed_error =
            try_ignored_by_rules("document.md", false, &malformed, false).unwrap_err();
        assert!(
            malformed_error
                .to_string()
                .contains("ignore pattern is empty or exceeds the policy limit")
        );

        let over_budget = [IgnoreRule {
            pattern: "a".repeat(MAX_GENERATED_PARENT_IGNORE_PATTERN_BYTES),
            negated: false,
            scope_prefix: None,
        }];
        let budget_error =
            try_ignored_by_rules(&"a".repeat(24), false, &over_budget, false).unwrap_err();
        assert!(
            budget_error
                .to_string()
                .contains("KIO-E-SCAN-IGNORE-BUDGET-001")
        );
    }

    #[test]
    fn r9_1_ignore_matches_across_unicode_normal_forms() {
        // R9-1: a canonically-equivalent ignore pattern must exclude a file whose
        // on-disk name uses a different Unicode normal form. Pre-fix the byte-wise
        // comparison missed this, so an NFD file name (routine on macOS/APFS)
        // slipped past an NFC `.kioignore` pattern and was indexed / sent online.
        let nfc_name = "café.md"; // é = U+00E9 (precomposed)
        let nfd_name = "cafe\u{0301}.md"; // e + U+0301 combining acute
        assert_ne!(
            nfc_name.as_bytes(),
            nfd_name.as_bytes(),
            "the two normal forms must differ byte-wise"
        );

        let nfc_rule = vec![IgnoreRule {
            pattern: nfc_name.to_owned(),
            negated: false,
            scope_prefix: None,
        }];
        let nfd_rule = vec![IgnoreRule {
            pattern: nfd_name.to_owned(),
            negated: false,
            scope_prefix: None,
        }];

        // NFC pattern excludes an NFD on-disk name, and vice versa.
        assert!(try_ignored_by_rules(nfd_name, false, &nfc_rule, false).unwrap());
        assert!(try_ignored_by_rules(nfc_name, false, &nfd_rule, false).unwrap());
        // The same-form case still works.
        assert!(try_ignored_by_rules(nfc_name, false, &nfc_rule, false).unwrap());
        // A genuinely different name is still not excluded.
        assert!(!try_ignored_by_rules("other.md", false, &nfc_rule, false).unwrap());
    }

    #[test]
    fn r10_3_case_insensitive_volume_folds_ignore_pattern() {
        // R10-3: a lowercase ignore pattern must exclude a differently-cased on-disk
        // name on a case-insensitive volume (where both names are the SAME file), and
        // must NOT fold on a case-sensitive volume (folding would wrongly exclude a
        // distinct file — silent data loss).
        let rule = vec![IgnoreRule {
            pattern: "casefixture.md".to_owned(),
            negated: false,
            scope_prefix: None,
        }];
        // Insensitive volume: case-different name is excluded.
        assert!(try_ignored_by_rules("CaseFixture.md", false, &rule, true).unwrap());
        // Sensitive volume: case-different name is NOT excluded.
        assert!(!try_ignored_by_rules("CaseFixture.md", false, &rule, false).unwrap());
        // Exact-case name is excluded on either volume.
        assert!(try_ignored_by_rules("casefixture.md", false, &rule, false).unwrap());
        assert!(try_ignored_by_rules("casefixture.md", false, &rule, true).unwrap());
        // A genuinely different name is never excluded, even when folding.
        assert!(!try_ignored_by_rules("other.md", false, &rule, true).unwrap());
        // Unicode-aware fold: an uppercase-É (U+00C9) pattern folds to match an é name.
        let unicode_rule = vec![IgnoreRule {
            pattern: "CAF\u{00c9}.md".to_owned(),
            negated: false,
            scope_prefix: None,
        }];
        assert!(try_ignored_by_rules("caf\u{00e9}.md", false, &unicode_rule, true).unwrap());
        assert!(!try_ignored_by_rules("caf\u{00e9}.md", false, &unicode_rule, false).unwrap());
        // Negation (`!keep`) unignores across case on an insensitive volume too.
        let negation = vec![
            IgnoreRule {
                pattern: "*.log".to_owned(),
                negated: false,
                scope_prefix: None,
            },
            IgnoreRule {
                pattern: "KEEP.log".to_owned(),
                negated: true,
                scope_prefix: None,
            },
        ];
        assert!(try_explicitly_unignored("keep.log", false, &negation, true).unwrap());
        assert!(!try_explicitly_unignored("keep.log", false, &negation, false).unwrap());
    }

    #[test]
    fn r10_3_probe_matches_actual_fs_case_behavior_and_cleans_up() {
        // The probe result must equal an independent ground-truth check on the SAME
        // volume, and the probe must never leave its files behind.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("groundtruth-a"), b"").unwrap();
        let fs_insensitive = dir.path().join("GROUNDTRUTH-A").exists();
        std::fs::remove_file(dir.path().join("groundtruth-a")).unwrap();
        assert_eq!(probe_case_insensitive(dir.path()), fs_insensitive);
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("kio-caseprobe")
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "probe must remove its files: {leftovers:?}"
        );
    }

    #[test]
    fn r23_cand_005_question_matches_one_unicode_scalar() {
        let rules = vec![IgnoreRule {
            pattern: "?.txt".to_owned(),
            negated: false,
            scope_prefix: None,
        }];
        for name in ["a.txt", "é.txt", "e\u{301}.txt", "😀.txt"] {
            assert!(
                try_ignored_by_rules(name, false, &rules, false).unwrap(),
                "one-scalar name should be ignored: {name:?}"
            );
        }
        assert!(!try_ignored_by_rules("ab.txt", false, &rules, false).unwrap());
        assert!(!wildcard_match("?", "/").unwrap());
    }

    #[test]
    fn r23_cand_005_unicode_rule_applies_through_scan_preview() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".kioignore"), "?.txt\n").unwrap();
        std::fs::write(dir.path().join("é.txt"), b"excluded").unwrap();
        std::fs::write(dir.path().join("ab.txt"), b"included").unwrap();

        let preview = build_scan_preview(ScanPreviewRequest {
            scope_path: dir.path().display().to_string(),
            include_raw_hashes: false,
            require_network_approval: false,
        })
        .unwrap();
        assert!(
            preview
                .candidates
                .iter()
                .find(|candidate| candidate.input_path == "é.txt")
                .unwrap()
                .ignored
        );
        assert!(
            !preview
                .candidates
                .iter()
                .find(|candidate| candidate.input_path == "ab.txt")
                .unwrap()
                .ignored
        );
    }

    #[test]
    fn r23_cand_017_adversarial_glob_visits_bounded_states() {
        let mut pattern = "*a".repeat(64);
        pattern.push('b');
        let value = "a".repeat(64);
        assert!(!wildcard_match(&pattern, &value).unwrap());

        let over_budget = "a".repeat(MAX_GLOB_STATES);
        let error = wildcard_match(&over_budget, "").unwrap_err();
        assert!(error.to_string().contains("KIO-E-SCAN-IGNORE-BUDGET-001"));
    }

    #[test]
    fn r23_cand_017_star_and_double_star_semantics_remain_intact() {
        assert!(wildcard_match("*.md", "notes.md").unwrap());
        assert!(!wildcard_match("*.md", "dir/notes.md").unwrap());
        assert!(wildcard_match("**/notes.md", "notes.md").unwrap());
        assert!(wildcard_match("**/notes.md", "a/b/notes.md").unwrap());
        assert!(!wildcard_match("**/notes.md", "a/b/notes.txt").unwrap());
    }

    #[test]
    fn r23_cand_032_scan_hash_streams_from_verified_handle() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = vec![0x5a; 256 * 1024];
        std::fs::write(dir.path().join("large.bin"), &bytes).unwrap();

        let preview = build_scan_preview(ScanPreviewRequest {
            scope_path: dir.path().display().to_string(),
            include_raw_hashes: true,
            require_network_approval: false,
        })
        .unwrap();
        let candidate = preview
            .candidates
            .iter()
            .find(|candidate| candidate.input_path == "large.bin")
            .unwrap();
        assert_eq!(candidate.size_bytes, bytes.len() as u64);
        assert_eq!(
            candidate.raw_hash.as_deref(),
            Some(hash_bytes(&bytes).as_str())
        );
    }

    #[test]
    fn r23_cand_027_verified_read_is_bounded_and_scope_local() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("small.txt"), b"hello").unwrap();

        let input = read_verified_scan_input(dir.path(), "small.txt", 5).unwrap();
        assert_eq!(input.bytes, b"hello");
        assert_eq!(input.raw_hash, hash_bytes(b"hello"));
        assert!(read_verified_scan_input(dir.path(), "small.txt", 4).is_err());
        assert!(read_verified_scan_input(dir.path(), "../small.txt", 5).is_err());
    }

    #[test]
    fn r23_cand_057_streaming_identity_honors_exact_limit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bounded.bin"), b"12345").unwrap();

        let identity = hash_verified_scan_input(dir.path(), "bounded.bin", 5).unwrap();
        assert_eq!(identity.size_bytes, 5);
        assert_eq!(identity.raw_hash, hash_bytes(b"12345"));
        assert!(hash_verified_scan_input(dir.path(), "bounded.bin", 4).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn r23_cand_027_verified_read_rejects_symlink_target() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), b"outside").unwrap();
        symlink(outside.path(), dir.path().join("inside.txt")).unwrap();

        let error = read_verified_scan_input(dir.path(), "inside.txt", 1024).unwrap_err();
        assert!(error.to_string().contains("KIO-E-SCAN-FILE-IDENTITY-001"));
    }

    #[cfg(unix)]
    #[test]
    fn r23_cand_027_replacement_after_listing_rejects_outside_handle() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inside.txt");
        std::fs::write(&path, b"inside").unwrap();
        let listed = std::fs::symlink_metadata(&path).unwrap();

        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), b"outside").unwrap();
        std::fs::remove_file(&path).unwrap();
        symlink(outside.path(), &path).unwrap();
        let outside_handle = File::open(&path).unwrap();

        let error = verify_opened_regular_file(&path, listed, outside_handle).unwrap_err();
        assert!(error.to_string().contains("KIO-E-SCAN-FILE-IDENTITY-001"));
    }

    #[test]
    fn r23_cand_067_current_ignore_policy_reauthorizes_durable_input() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("private.pdf"), b"%PDF BT (text)").unwrap();
        let raw_hash = hash_bytes(b"%PDF BT (text)");

        assert!(current_scan_allows_file(dir.path(), "private.pdf", &raw_hash, 64).unwrap());
        assert!(
            !current_scan_allows_file(dir.path(), "private.pdf", &hash_bytes(b"other"), 64)
                .unwrap()
        );
        std::fs::write(dir.path().join(".kioignore"), "private.pdf\n").unwrap();
        assert!(!current_scan_allows_file(dir.path(), "private.pdf", &raw_hash, 64).unwrap());

        std::fs::write(dir.path().join(".kioignore"), "").unwrap();
        assert!(current_scan_allows_file(dir.path(), "private.pdf", &raw_hash, 13).is_err());
    }

    #[test]
    fn qb15_parent_ignore_is_prefix_qualified_for_child_and_grandchild_scans() {
        let parent = tempfile::tempdir().unwrap();
        let child = parent.path().join("project");
        let nested = child.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(parent.path().join(".kio")).unwrap();
        std::fs::write(
            parent.path().join(".kio/config.toml"),
            "[scope]\nignore = [\"project/private.md\", \"project/nested/secret.md\", \"!project/nested/keep.md\"]\n",
        )
        .unwrap();
        std::fs::write(child.join("private.md"), b"private").unwrap();
        std::fs::write(nested.join("secret.md"), b"secret").unwrap();
        std::fs::write(nested.join("keep.md"), b"keep").unwrap();

        let parent_plan = discover_child_scopes(parent.path()).unwrap();
        let child_rules = generated_parent_policy_for_child(&parent_plan, "project").unwrap();
        let child_preview = build_scan_preview_with_inherited_rules(
            ScanPreviewRequest {
                scope_path: child.display().to_string(),
                include_raw_hashes: false,
                require_network_approval: false,
            },
            &child_rules,
        )
        .unwrap();
        assert!(
            child_preview
                .candidates
                .iter()
                .find(|candidate| candidate.input_path == "private.md")
                .unwrap()
                .ignored
        );

        // Persist exactly the wire shape a child receives, then prove the next
        // discovery composes the prefix rather than losing the ancestor policy.
        std::fs::create_dir_all(child.join(".kio")).unwrap();
        let generated = toml::to_string(
            &serde_json::json!({ "generated_parent_policy": { "rules": child_rules } }),
        )
        .unwrap();
        std::fs::write(child.join(".kio/config.toml"), generated).unwrap();
        let child_plan = discover_child_scopes(&child).unwrap();
        let grandchild_rules = generated_parent_policy_for_child(&child_plan, "nested").unwrap();
        let nested_preview = build_scan_preview_with_inherited_rules(
            ScanPreviewRequest {
                scope_path: nested.display().to_string(),
                include_raw_hashes: false,
                require_network_approval: false,
            },
            &grandchild_rules,
        )
        .unwrap();
        assert!(
            nested_preview
                .candidates
                .iter()
                .find(|candidate| candidate.input_path == "secret.md")
                .unwrap()
                .ignored
        );
        assert!(
            !nested_preview
                .candidates
                .iter()
                .find(|candidate| candidate.input_path == "keep.md")
                .unwrap()
                .ignored
        );
    }

    #[test]
    fn qb15_malformed_generated_parent_policy_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".kio")).unwrap();
        std::fs::write(dir.path().join("document.md"), b"content").unwrap();
        std::fs::write(
            dir.path().join(".kio/config.toml"),
            "[generated_parent_policy]\nunknown = true\n",
        )
        .unwrap();
        assert!(
            build_scan_preview(ScanPreviewRequest {
                scope_path: dir.path().display().to_string(),
                include_raw_hashes: false,
                require_network_approval: false,
            })
            .is_err()
        );
    }

    #[test]
    fn qb15_persisted_parent_policy_reauthorizes_later_child_only_task_checks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".kio")).unwrap();
        std::fs::write(dir.path().join("private.md"), b"private").unwrap();
        std::fs::write(
            dir.path().join(".kio/config.toml"),
            "[generated_parent_policy]\n\n[[generated_parent_policy.rules]]\npattern = \"project/private.md\"\nnegated = false\nscope_prefix = \"project\"\n",
        )
        .unwrap();
        assert!(!current_scan_policy_allows_file(dir.path(), "private.md").unwrap());
    }

    #[test]
    fn ignored_files_do_not_prevent_management_of_their_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("private")).unwrap();
        std::fs::create_dir_all(dir.path().join(".kio")).unwrap();
        std::fs::write(dir.path().join("private/only.md"), b"private").unwrap();
        std::fs::write(
            dir.path().join(".kio/config.toml"),
            "[scope]\nignore = [\"private/only.md\"]\n",
        )
        .unwrap();
        let plan = discover_child_scopes(dir.path()).unwrap();
        assert!(
            plan.candidates
                .iter()
                .any(|candidate| candidate.path == "private" && candidate.status == "planned")
        );
    }
}
