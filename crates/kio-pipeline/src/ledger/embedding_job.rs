//! Private, immutable embedding recovery evidence, independent of tasks.jsonl.
use std::{collections::BTreeSet, path::PathBuf};

use kio_adapter::types::{EmbeddingCompleted, EmbeddingLogicalJobId, EmbeddingMemberResult};
use serde::{Deserialize, Serialize};

use super::{
    BatchRequestRow, LedgerDb, TaskKey, lifecycle::LedgerWriteTxn, lifecycle_fs::FsSession,
};
use crate::{PipelineError, Result, task::EmbeddingTaskTarget};

const MAX_ARTIFACT_BYTES: u64 = 32 * 1024 * 1024;
const MAX_MEMBERS: usize = 4096;
const MAX_CLEANUP_REFS: usize = 257;
const MAX_CLEANUP_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingJobMember {
    pub id: String,
    pub target: EmbeddingTaskTarget,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingJobBinding {
    pub logical_job_id: EmbeddingLogicalJobId,
    pub local: bool,
    pub dimensions: u32,
    pub distance: String,
    pub modality: String,
    pub members: Vec<EmbeddingJobMember>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BoundIntent {
    version: u8,
    ledger_id: String,
    era: String,
    key: TaskKey,
    submission_seq: i64,
    intent_token: String,
    binding: EmbeddingJobBinding,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BoundResult {
    version: u8,
    intent_digest: String,
    result_digest: String,
    result: EmbeddingCompleted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CleanupRef {
    key: TaskKey,
    submission_seq: i64,
    intent_token: String,
    binding_digest: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CleanupManifest {
    version: u8,
    ledger_id: String,
    era: String,
    entries: Vec<CleanupRef>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingJob {
    pub key: TaskKey,
    pub submission_seq: i64,
    pub intent_token: String,
    pub binding: EmbeddingJobBinding,
    pub known_result: Option<EmbeddingCompleted>,
}

pub struct EmbeddingReserveRequest<'a> {
    pub key: &'a TaskKey,
    pub candidate_usd: f64,
    pub caps: &'a super::ops::BudgetCapConfig,
    pub request_kind: super::RequestKind,
    pub timeout_seconds: Option<i64>,
    pub bypass_cap_denial: bool,
    pub binding: &'a EmbeddingJobBinding,
}

pub struct EmbeddingDeviceClaimRequest<'a> {
    pub key: &'a TaskKey,
    pub now_ms: i64,
    pub candidate_usd: f64,
    pub timeout_seconds: i64,
    pub device_cap: f64,
    pub per_adapter_cap: Option<f64>,
    pub binding: &'a EmbeddingJobBinding,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EmbeddingReservationOutcome {
    Reserved(EmbeddingJob),
    KnownResult(EmbeddingJob),
    InFlight(EmbeddingJob),
    BudgetExceeded,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EmbeddingDeviceClaimOutcome {
    Claimed(Box<EmbeddingJob>),
    InFlight,
    Denied(super::ops::CapLayer),
}

fn invalid(message: &str) -> PipelineError {
    PipelineError::contract("KIO-E-EMBEDDING-JOURNAL-001", message)
}

fn canonical<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_jcs::to_vec(value).map_err(|_| invalid("cannot encode embedding recovery evidence"))
}

fn digest(bytes: &[u8]) -> String {
    kio_core::cas::hash_bytes(bytes)
        .trim_start_matches("sha256:")
        .to_owned()
}

fn locator(ledger_id: &str, era: &str, key: &TaskKey, seq: i64, kind: &str) -> Result<PathBuf> {
    if seq <= 0
        || [ledger_id, era]
            .iter()
            .any(|id| id.is_empty() || !id.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(invalid("invalid ledger authority or submission sequence"));
    }
    Ok(PathBuf::from(format!(
        "embedding-job-{ledger_id}-{era}-{}-{seq}.{kind}.json",
        digest(&canonical(key)?)
    )))
}

fn cleanup_locator(ledger_id: &str, era: &str) -> PathBuf {
    PathBuf::from(format!("embedding-cleanup-{ledger_id}-{era}.json"))
}

fn read_cleanup(fs: &FsSession, ledger_id: &str, era: &str) -> Result<CleanupManifest> {
    let Some(bytes) =
        fs.read_optional_private(&cleanup_locator(ledger_id, era), MAX_CLEANUP_BYTES)?
    else {
        return Ok(CleanupManifest {
            version: 1,
            ledger_id: ledger_id.to_owned(),
            era: era.to_owned(),
            entries: Vec::new(),
        });
    };
    let manifest: CleanupManifest = serde_json::from_slice(&bytes)
        .map_err(|_| invalid("invalid embedding cleanup manifest"))?;
    if manifest.version != 1
        || manifest.ledger_id != ledger_id
        || manifest.era != era
        || manifest.entries.len() > MAX_CLEANUP_REFS
        || canonical(&manifest)? != bytes
    {
        return Err(invalid("embedding cleanup authority or bound mismatch"));
    }
    let mut identities = BTreeSet::new();
    for entry in &manifest.entries {
        if entry.key.adapter_kind != "embedding"
            || !kio_core::cas::is_hash(&entry.key.input_hash)
            || !kio_core::cas::is_hash(&entry.key.tool_profile_hash)
            || entry.submission_seq <= 0
            || !super::ops::is_uuid_v7(&entry.intent_token)
            || entry.binding_digest.len() != 64
            || !entry
                .binding_digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || !identities.insert((&entry.key, entry.submission_seq))
        {
            return Err(invalid("invalid embedding cleanup identity"));
        }
    }
    Ok(manifest)
}

pub(super) fn enqueue_cleanup(tx: &LedgerWriteTxn<'_>, jobs: &[EmbeddingJob]) -> Result<()> {
    if jobs.is_empty() {
        return Ok(());
    }
    let mut manifest = read_cleanup(tx.fs, tx.ledger_id, tx.era)?;
    for job in jobs {
        let leaf = locator(
            tx.ledger_id,
            tx.era,
            &job.key,
            job.submission_seq,
            "binding",
        )?;
        let bytes = tx.fs.read_private(&leaf, MAX_ARTIFACT_BYTES)?;
        let entry = CleanupRef {
            key: job.key.clone(),
            submission_seq: job.submission_seq,
            intent_token: job.intent_token.clone(),
            binding_digest: digest(&bytes),
        };
        if !manifest.entries.contains(&entry) {
            manifest.entries.push(entry);
        }
    }
    if manifest.entries.len() > MAX_CLEANUP_REFS {
        return Err(invalid("embedding cleanup queue capacity exceeded"));
    }
    tx.fs.write_embedding_cleanup_manifest(
        &cleanup_locator(tx.ledger_id, tx.era),
        &canonical(&manifest)?,
    )
}

/// Drain one fixed authority-bound manifest; never enumerate filesystem leaves.
/// A prepared entry without a committed terminal cost is acknowledged without
/// deleting its binding. A future stale sweep can enqueue it again.
pub(super) fn drain_cleanup(db: &LedgerDb) -> Result<()> {
    db.read_artifacts(|conn, fs, ledger_id, era| {
        let mut manifest = read_cleanup(fs, ledger_id, era)?;
        if manifest.entries.is_empty() {
            return Ok(());
        }
        for entry in &manifest.entries {
            let leaf = locator(ledger_id, era, &entry.key, entry.submission_seq, "binding")?;
            let Some(bytes) = fs.read_optional_private(&leaf, MAX_ARTIFACT_BYTES)? else {
                continue;
            };
            let intent: BoundIntent = serde_json::from_slice(&bytes)
                .map_err(|_| invalid("invalid queued embedding binding"))?;
            if digest(&bytes) != entry.binding_digest
                || canonical(&intent)? != bytes
                || intent.version != 1
                || intent.ledger_id != ledger_id
                || intent.era != era
                || intent.key != entry.key
                || intent.submission_seq != entry.submission_seq
                || intent.intent_token != entry.intent_token
            {
                return Err(invalid("queued embedding binding identity mismatch"));
            }
            validate_binding(&intent.key, &intent.binding)?;
            let costs = super::ops_sql::cost_ledger_rows_for_key(conn, &entry.key)?;
            if !terminal_proof(
                &costs,
                &entry.key,
                entry.submission_seq,
                &entry.intent_token,
                intent.binding.local,
            ) {
                continue;
            }
            let result = locator(ledger_id, era, &entry.key, entry.submission_seq, "result")?;
            if fs
                .read_optional_private(&result, MAX_ARTIFACT_BYTES)?
                .is_some()
            {
                return Err(invalid(
                    "known results require explicit publication before cleanup",
                ));
            }
            fs.remove_embedding_artifact(&leaf, &bytes, MAX_ARTIFACT_BYTES)?;
        }
        manifest.entries.clear();
        fs.write_embedding_cleanup_manifest(
            &cleanup_locator(ledger_id, era),
            &canonical(&manifest)?,
        )
    })
}

fn terminal_proof(
    costs: &[super::CostLedgerRow],
    key: &TaskKey,
    seq: i64,
    token: &str,
    local: bool,
) -> bool {
    costs.iter().any(|cost| {
        cost.key == *key
            && ((cost.submission_seq == seq && (!local || cost.batch_job_id == token))
                || (seq.checked_add(1) == Some(cost.submission_seq)
                    && cost.outcome == super::Outcome::UnknownSettled
                    && cost.batch_job_id == token))
    })
}

fn validate_binding(key: &TaskKey, binding: &EmbeddingJobBinding) -> Result<()> {
    if key.adapter_kind != "embedding"
        || !kio_core::cas::is_hash(&key.input_hash)
        || !kio_core::cas::is_hash(&key.tool_profile_hash)
        || binding.members.is_empty()
        || binding.members.len() > MAX_MEMBERS
        || binding.dimensions == 0
        || binding.dimensions > 65536
        || binding.distance.is_empty()
        || binding.distance.len() > 64
        || binding.modality.is_empty()
        || binding.modality.len() > 64
        || binding.logical_job_id.0.is_empty()
        || binding.logical_job_id.0.len() > 256
        || !binding
            .logical_job_id
            .0
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_:.".contains(&b))
    {
        return Err(invalid("invalid embedding job identity"));
    }
    let mut ids = BTreeSet::new();
    for member in &binding.members {
        if member.id.is_empty()
            || member.id.len() > 256
            || !member
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_:.".contains(&b))
            || !ids.insert(&member.id)
            || !member.target.is_valid()
            || member.target.profile_hash() != key.tool_profile_hash
            || std::mem::discriminant(&member.target)
                != std::mem::discriminant(&binding.members[0].target)
        {
            return Err(invalid(
                "invalid, duplicate, or cross-profile embedding member",
            ));
        }
    }
    Ok(())
}

/// Reserve and fsync the original member binding before the phase-1 SQL commit.
/// Returning an existing job never authorizes another submit.
pub fn reserve_embedding_job(
    db: &LedgerDb,
    request: &EmbeddingReserveRequest<'_>,
) -> Result<EmbeddingReservationOutcome> {
    drain_cleanup(db)?;
    let EmbeddingReserveRequest {
        key,
        candidate_usd,
        caps,
        request_kind,
        timeout_seconds,
        bypass_cap_denial,
        binding,
    } = *request;
    validate_binding(key, binding)?;
    if key.is_device() || matches!(binding.members[0].target, EmbeddingTaskTarget::Query { .. }) {
        return Err(invalid(
            "query embeddings require the device claim operation",
        ));
    }
    if binding.local && (candidate_usd != 0.0 || request_kind != super::RequestKind::Sync) {
        return Err(invalid(
            "local embedding must have a zero-cost sync reservation",
        ));
    }
    let outcome = db.write(|tx| {
        let mut replaced_terminal = None;
        if let Some(row) = super::ops_sql::get_batch_request(tx.connection(), key)? {
            if let Some(job) = load(tx.fs, tx.ledger_id, tx.era, &row, tx.connection())? {
                if job.known_result.is_some() {
                    return Ok(EmbeddingReservationOutcome::KnownResult(job));
                }
                if row.state.is_inflight() {
                    return Ok(EmbeddingReservationOutcome::InFlight(job));
                }
                replaced_terminal = Some(job);
            } else if row.state.is_inflight() {
                return Err(invalid(
                    "in-flight embedding has no original member binding",
                ));
            }
        }
        super::ops_sql::guard_unauthorized_unknown_sync_resend(tx.connection(), key)?;
        let cap = super::ops_sql::check_then_reserve(
            tx.connection(),
            tx.control(),
            key,
            candidate_usd,
            caps,
            request_kind,
            timeout_seconds,
        )?;
        if matches!(cap, super::ops::CapCheckResult::Denied(_)) {
            if !bypass_cap_denial {
                return Ok(EmbeddingReservationOutcome::BudgetExceeded);
            }
            super::ops_sql::phase1_intent(
                tx.connection(),
                tx.control(),
                key,
                request_kind,
                candidate_usd,
                timeout_seconds,
            )?;
        }
        let row = super::ops_sql::get_batch_request(tx.connection(), key)?
            .ok_or_else(|| invalid("new embedding reservation missing"))?;
        if let Some(previous) = replaced_terminal {
            enqueue_cleanup(tx, &[previous])?;
        }
        bind(tx, &row, binding).map(EmbeddingReservationOutcome::Reserved)
    })?;
    drain_cleanup(db)?;
    Ok(outcome)
}

/// Query claims remain independent user executions. Inline remote stale
/// recovery keeps its conservative unknown settlement; no query vector replay
/// is retained here. Only the current claim's body-free binding is persisted.
pub fn claim_device_embedding_job(
    db: &LedgerDb,
    request: &EmbeddingDeviceClaimRequest<'_>,
) -> Result<EmbeddingDeviceClaimOutcome> {
    drain_cleanup(db)?;
    let EmbeddingDeviceClaimRequest {
        key,
        now_ms,
        candidate_usd,
        timeout_seconds,
        device_cap,
        per_adapter_cap,
        binding,
    } = *request;
    validate_binding(key, binding)?;
    if !key.is_device()
        || binding.members.len() != 1
        || !matches!(binding.members[0].target, EmbeddingTaskTarget::Query { .. })
        || binding.members[0].target.input_hash() != key.input_hash
        || (binding.local && candidate_usd != 0.0)
    {
        return Err(invalid("invalid device query binding"));
    }
    let outcome = db.write(|tx| {
        let replaced_terminal = terminal_replacement_job(tx, key)?;
        let plan = super::ops_sql::plan_bounded_sweep(tx.connection(), Some(key), now_ms)?;
        sweep_in_tx(tx, &plan, now_ms)?;
        let outcome = super::ops_sql::device_claim(
            tx.connection(),
            tx.control(),
            key,
            candidate_usd,
            timeout_seconds,
            device_cap,
            per_adapter_cap,
        )?;
        let value = match outcome {
            super::ops::ClaimOutcome::Claimed(_) => {
                if let Some(previous) = replaced_terminal {
                    enqueue_cleanup(tx, &[previous])?;
                }
                let row = super::ops_sql::get_batch_request(tx.connection(), key)?
                    .ok_or_else(|| invalid("device embedding claim disappeared"))?;
                EmbeddingDeviceClaimOutcome::Claimed(Box::new(bind(tx, &row, binding)?))
            }
            super::ops::ClaimOutcome::InFlight => EmbeddingDeviceClaimOutcome::InFlight,
            super::ops::ClaimOutcome::Denied(layer) => EmbeddingDeviceClaimOutcome::Denied(layer),
        };
        Ok(value)
    })?;
    drain_cleanup(db)?;
    Ok(outcome)
}

/// Typed embedding-only remote transition. The caller already checked the exact
/// credential-bound provider scope and adapter response attribution.
pub(super) fn record_embedding_pending(
    db: &LedgerDb,
    job: &EmbeddingJob,
    outcome: &kio_adapter::types::EmbeddingOutcome,
) -> Result<bool> {
    let kio_adapter::types::EmbeddingOutcome::Pending { remote_handle } = outcome else {
        return Err(invalid("expected a Pending embedding outcome"));
    };
    if job.binding.local
        || job.key.is_device()
        || matches!(
            job.binding.members.first().map(|member| &member.target),
            Some(EmbeddingTaskTarget::Query { .. })
        )
        || remote_handle.0.trim().is_empty()
    {
        return Err(invalid(
            "local, query, or malformed execution cannot acquire a remote handle",
        ));
    }
    validate_binding(&job.key, &job.binding)?;
    db.write(|tx| {
        let Some(row) = super::ops_sql::get_batch_request(tx.connection(), &job.key)? else {
            return Ok(false);
        };
        if row.submission_seq != job.submission_seq
            || row.intent_token.as_deref() != Some(&job.intent_token)
            || !row.state.is_inflight()
        {
            return Ok(false);
        }
        if row.request_kind != super::RequestKind::Batch {
            return Err(invalid(
                "Pending embedding outcomes require a Batch reservation",
            ));
        }
        let original = load(tx.fs, tx.ledger_id, tx.era, &row, tx.connection())?
            .ok_or_else(|| invalid("Pending embedding job has no frozen original binding"))?;
        if original.binding != job.binding || original.known_result.is_some() {
            return Err(invalid(
                "Pending embedding job does not match its frozen original binding",
            ));
        }
        if row
            .provider_scope_id
            .as_deref()
            .is_none_or(|scope| scope.trim().is_empty())
            || row.job_create_started_at.is_none()
        {
            return Err(invalid(
                "Pending embedding job requires a recorded provider scope and creation intent",
            ));
        }
        if let Some(existing) = row.batch_job_id.as_deref() {
            if existing != remote_handle.0 || row.state != super::BatchState::JobCreated {
                return Err(invalid(
                    "Pending embedding handle conflicts with its original remote job",
                ));
            }
            return Ok(false);
        }
        if row.state != super::BatchState::Intent {
            return Err(invalid(
                "Pending embedding transition has an invalid source state",
            ));
        }
        super::ops_sql::phase2b_record_job_created(
            tx.connection(),
            &job.key,
            &job.intent_token,
            &remote_handle.0,
        )
    })
}

fn validate_result(binding: &EmbeddingJobBinding, result: &EmbeddingCompleted) -> Result<()> {
    if result.dimensions != binding.dimensions
        || result.distance != binding.distance
        || result.embedding_profile_hash != binding.members[0].target.profile_hash()
        || result.modality != binding.modality
        || result.members.len() != binding.members.len()
    {
        return Err(invalid(
            "known result does not match frozen embedding contract",
        ));
    }
    let expected: BTreeSet<_> = binding.members.iter().map(|m| m.id.as_str()).collect();
    let mut seen = BTreeSet::new();
    for member in &result.members {
        if !expected.contains(member.id.as_str()) || !seen.insert(member.id.as_str()) {
            return Err(invalid("known result member mapping is invalid"));
        }
        match &member.result {
            EmbeddingMemberResult::Vector { vector } => {
                let norm: f64 = vector
                    .iter()
                    .map(|v| f64::from(*v).powi(2))
                    .sum::<f64>()
                    .sqrt();
                if vector.len() != binding.dimensions as usize
                    || vector.iter().any(|v| !v.is_finite())
                    || !norm.is_finite()
                    || (norm - 1.0).abs() > 0.0001
                {
                    return Err(invalid(
                        "known result vector must be validated and normalized",
                    ));
                }
            }
            EmbeddingMemberResult::Failed { reason }
                if reason.is_empty()
                    || reason.len() > 256
                    || reason.chars().any(char::is_control) =>
            {
                return Err(invalid("invalid known member failure classification"));
            }
            EmbeddingMemberResult::Failed { .. } => {}
        }
    }
    Ok(())
}

fn persist(fs: &FsSession, leaf: &std::path::Path, bytes: &[u8]) -> Result<()> {
    if bytes.len() as u64 > MAX_ARTIFACT_BYTES {
        return Err(invalid("embedding recovery evidence exceeds limit"));
    }
    if let Some(existing) = fs.read_optional_private(leaf, MAX_ARTIFACT_BYTES)? {
        if existing != bytes {
            return Err(invalid("immutable embedding evidence differs"));
        }
        return Ok(());
    }
    fs.create_embedding_artifact(leaf, bytes)
}

pub(super) fn bind(
    tx: &LedgerWriteTxn<'_>,
    row: &BatchRequestRow,
    binding: &EmbeddingJobBinding,
) -> Result<EmbeddingJob> {
    validate_binding(&row.key, binding)?;
    let intent = BoundIntent {
        version: 1,
        ledger_id: tx.ledger_id.to_owned(),
        era: tx.era.to_owned(),
        key: row.key.clone(),
        submission_seq: row.submission_seq,
        intent_token: row
            .intent_token
            .clone()
            .ok_or_else(|| invalid("reservation has no intent"))?,
        binding: binding.clone(),
    };
    let leaf = locator(
        tx.ledger_id,
        tx.era,
        &row.key,
        row.submission_seq,
        "binding",
    )?;
    let bytes = canonical(&intent)?;
    if let Some(existing) = tx.fs.read_optional_private(&leaf, MAX_ARTIFACT_BYTES)?
        && existing != bytes
    {
        // Only a fresh phase 1 calls bind. A committed previous attempt would
        // advance the sequence, so same-seq/different-token evidence is orphaned.
        let old: BoundIntent = serde_json::from_slice(&existing)
            .map_err(|_| invalid("prepared embedding evidence is malformed"))?;
        let result_leaf = locator(tx.ledger_id, tx.era, &row.key, row.submission_seq, "result")?;
        let costs = super::ops_sql::cost_ledger_rows_for_key(tx.connection(), &row.key)?;
        if old.version != 1
            || old.ledger_id != tx.ledger_id
            || old.era != tx.era
            || old.key != row.key
            || old.submission_seq != row.submission_seq
            || old.intent_token == intent.intent_token
            || !super::ops::is_uuid_v7(&old.intent_token)
            || canonical(&old)? != existing
            || tx
                .fs
                .read_optional_private(&result_leaf, MAX_ARTIFACT_BYTES)?
                .is_some()
            || costs
                .iter()
                .any(|cost| cost.submission_seq == row.submission_seq)
        {
            return Err(invalid(
                "prepared embedding evidence cannot safely be replaced",
            ));
        }
        validate_binding(&old.key, &old.binding)?;
        tx.fs
            .remove_embedding_artifact(&leaf, &existing, MAX_ARTIFACT_BYTES)?;
    }
    persist(tx.fs, &leaf, &bytes)?;
    load(tx.fs, tx.ledger_id, tx.era, row, tx.connection())?
        .ok_or_else(|| invalid("binding publication disappeared"))
}

fn load(
    fs: &FsSession,
    ledger_id: &str,
    era: &str,
    row: &BatchRequestRow,
    conn: &rusqlite::Connection,
) -> Result<Option<EmbeddingJob>> {
    let leaf = locator(ledger_id, era, &row.key, row.submission_seq, "binding")?;
    let Some(bytes) = fs.read_optional_private(&leaf, MAX_ARTIFACT_BYTES)? else {
        return Ok(None);
    };
    let intent: BoundIntent =
        serde_json::from_slice(&bytes).map_err(|_| invalid("invalid embedding binding"))?;
    if intent.version != 1
        || intent.ledger_id != ledger_id
        || intent.era != era
        || intent.key != row.key
        || intent.submission_seq != row.submission_seq
        || !super::ops::is_uuid_v7(&intent.intent_token)
        || canonical(&intent)? != bytes
    {
        return Err(invalid("embedding binding authority mismatch"));
    }
    validate_binding(&intent.key, &intent.binding)?;
    if row.state.is_inflight() {
        if row.intent_token.as_deref() != Some(&intent.intent_token) {
            return Err(invalid("embedding reservation token mismatch"));
        }
    } else {
        let costs = super::ops_sql::cost_ledger_rows_for_key(conn, &row.key)?;
        let audit_id = row.batch_job_id.as_deref().unwrap_or(&intent.intent_token);
        if !costs
            .iter()
            .any(|cost| cost.submission_seq == row.submission_seq && cost.batch_job_id == audit_id)
        {
            return Err(invalid(
                "terminal embedding binding has no settlement receipt",
            ));
        }
    }
    let result_leaf = locator(ledger_id, era, &row.key, row.submission_seq, "result")?;
    let known_result = fs
        .read_optional_private(&result_leaf, MAX_ARTIFACT_BYTES)?
        .map(|bytes| {
            let result: BoundResult =
                serde_json::from_slice(&bytes).map_err(|_| invalid("invalid known result"))?;
            if result.version != 1
                || result.intent_digest != digest(&canonical(&intent)?)
                || result.result_digest != digest(&canonical(&result.result)?)
                || canonical(&result)? != bytes
            {
                return Err(invalid("known result digest mismatch"));
            }
            validate_result(&intent.binding, &result.result)?;
            Ok(result.result)
        })
        .transpose()?;
    Ok(Some(EmbeddingJob {
        key: intent.key,
        submission_seq: intent.submission_seq,
        intent_token: intent.intent_token,
        binding: intent.binding,
        known_result,
    }))
}

/// Locate evidence through committed ledger state, including a terminal row
/// whose token was cleared by the atomic settlement transaction.
pub fn embedding_job(db: &LedgerDb, row: &BatchRequestRow) -> Result<Option<EmbeddingJob>> {
    db.read_artifacts(|conn, fs, ledger_id, era| {
        let current = super::ops_sql::get_batch_request(conn, &row.key)?
            .ok_or_else(|| invalid("embedding reservation no longer exists"))?;
        if current.submission_seq != row.submission_seq {
            return Err(invalid("embedding attempt changed"));
        }
        load(fs, ledger_id, era, &current, conn)
    })
}

/// Recover staged document results across task loss and profile changes. The
/// SQL cursor streams every current key; only the returned known jobs are
/// capped, so old cleaned rows cannot starve later unpublished results.
pub fn pending_embedding_publications_for_scope(
    db: &LedgerDb,
    scope_id: &str,
) -> Result<Vec<EmbeddingJob>> {
    if scope_id == TaskKey::DEVICE_SCOPE_ID {
        return Err(invalid(
            "query results are ephemeral and have no publication recovery",
        ));
    }
    db.read_artifacts(|conn, fs, ledger_id, era| {
        let mut statement = conn.prepare("SELECT scope_id, adapter_kind, input_hash, tool_profile_hash FROM batch_requests WHERE scope_id=?1 AND adapter_kind='embedding' ORDER BY scope_id, adapter_kind, input_hash, tool_profile_hash")?;
        let keys = statement.query_map([scope_id], |row| Ok(TaskKey::new(
            row.get::<_,String>(0)?, row.get::<_,String>(1)?, row.get::<_,String>(2)?, row.get::<_,String>(3)?,
        )))?;
        let mut jobs = Vec::new();
        for key in keys {
            let row = super::ops_sql::get_batch_request(conn, &key?)?
                .ok_or_else(|| invalid("publication recovery row disappeared"))?;
            if let Some(job) = load(fs, ledger_id, era, &row, conn)?
                && job.known_result.is_some() {
                jobs.push(job);
                if jobs.len() == 256 { break; }
            }
        }
        Ok(jobs)
    })
}

/// Durably retain already validated vectors and usage before settlement.
pub fn stage_embedding_result(
    db: &LedgerDb,
    job: &EmbeddingJob,
    result: &EmbeddingCompleted,
) -> Result<()> {
    drain_cleanup(db)?;
    if job.key.is_device() {
        return Err(invalid("query vectors are ephemeral and cannot be staged"));
    }
    db.write(|tx| {
        let row = super::ops_sql::get_batch_request(tx.connection(), &job.key)?
            .ok_or_else(|| invalid("embedding reservation missing"))?;
        let current = load(tx.fs, tx.ledger_id, tx.era, &row, tx.connection())?
            .ok_or_else(|| invalid("embedding binding missing"))?;
        if current.submission_seq != job.submission_seq
            || current.intent_token != job.intent_token
            || current.binding != job.binding
        {
            return Err(invalid("embedding job identity changed"));
        }
        if !row.state.is_inflight() && current.known_result.is_none() {
            return Err(invalid("cannot attach a new result to a terminal attempt"));
        }
        validate_result(&current.binding, result)?;
        let intent_leaf = locator(
            tx.ledger_id,
            tx.era,
            &job.key,
            job.submission_seq,
            "binding",
        )?;
        let intent = tx.fs.read_private(&intent_leaf, MAX_ARTIFACT_BYTES)?;
        let staged = BoundResult {
            version: 1,
            intent_digest: digest(&intent),
            result_digest: digest(&canonical(result)?),
            result: result.clone(),
        };
        persist(
            tx.fs,
            &locator(tx.ledger_id, tx.era, &job.key, job.submission_seq, "result")?,
            &canonical(&staged)?,
        )
    })
}

/// Remove recovery artifacts only after every publication and task projection
/// has completed. The durable terminal cost row remains the audit authority.
pub fn finish_embedding_publication(db: &LedgerDb, job: &EmbeddingJob) -> Result<()> {
    drain_cleanup(db)?;
    db.read_artifacts(|conn, fs, ledger_id, era| {
        let costs = super::ops_sql::cost_ledger_rows_for_key(conn, &job.key)?;
        let proof = terminal_proof(
            &costs,
            &job.key,
            job.submission_seq,
            &job.intent_token,
            job.binding.local,
        );
        if !proof {
            return Err(invalid(
                "embedding publication requires its durable terminal audit",
            ));
        }
        let binding_leaf = locator(ledger_id, era, &job.key, job.submission_seq, "binding")?;
        if let Some(bytes) = fs.read_optional_private(&binding_leaf, MAX_ARTIFACT_BYTES)? {
            let intent: BoundIntent = serde_json::from_slice(&bytes)
                .map_err(|_| invalid("invalid embedding publication binding"))?;
            if intent.version != 1
                || intent.ledger_id != ledger_id
                || intent.era != era
                || intent.key != job.key
                || intent.submission_seq != job.submission_seq
                || intent.intent_token != job.intent_token
                || intent.binding != job.binding
                || canonical(&intent)? != bytes
            {
                return Err(invalid("embedding publication identity changed"));
            }
            validate_binding(&intent.key, &intent.binding)?;
            for kind in ["result", "binding"] {
                let leaf = locator(ledger_id, era, &job.key, job.submission_seq, kind)?;
                if let Some(bytes) = fs.read_optional_private(&leaf, MAX_ARTIFACT_BYTES)? {
                    fs.remove_embedding_artifact(&leaf, &bytes, MAX_ARTIFACT_BYTES)?;
                }
            }
        }
        Ok(())
    })
}

pub(super) fn terminal_replacement_job(
    tx: &LedgerWriteTxn<'_>,
    key: &TaskKey,
) -> Result<Option<EmbeddingJob>> {
    let Some(row) = super::ops_sql::get_batch_request(tx.connection(), key)? else {
        return Ok(None);
    };
    if row.state.is_inflight() {
        return Ok(None);
    }
    Ok(load(tx.fs, tx.ledger_id, tx.era, &row, tx.connection())?
        .filter(|job| job.known_result.is_none()))
}

pub(super) fn sweep_in_tx(
    tx: &LedgerWriteTxn<'_>,
    plan: &super::ops::SweepPlan,
    now_ms: i64,
) -> Result<super::ops::SweepReport> {
    let mut filtered = plan.clone();
    let mut cleanup = Vec::new();
    let mut local_settled = Vec::new();
    for keys in [&mut filtered.own_key_stale, &mut filtered.general_stale] {
        let mut keep = Vec::new();
        for key in keys.drain(..) {
            let Some(row) = super::ops_sql::get_batch_request(tx.connection(), &key)? else {
                continue;
            };
            match load(tx.fs, tx.ledger_id, tx.era, &row, tx.connection())? {
                Some(job) if job.known_result.is_some() && !job.key.is_device() => {}
                Some(job) if job.binding.local => {
                    settle_local_crash_in_tx(tx, &job)?;
                    local_settled.push(key);
                    cleanup.push(job);
                }
                Some(job) => {
                    cleanup.push(job);
                    keep.push(key);
                }
                None => keep.push(key),
            }
        }
        *keys = keep;
    }
    let mut prune = Vec::new();
    for key in filtered.prune.drain(..) {
        let Some(row) = super::ops_sql::get_batch_request(tx.connection(), &key)? else {
            continue;
        };
        match load(tx.fs, tx.ledger_id, tx.era, &row, tx.connection())? {
            Some(job) if job.key.is_device() || job.known_result.is_none() => {
                cleanup.push(job);
                prune.push(key);
            }
            Some(_) => {}
            None => prune.push(key),
        }
    }
    filtered.prune = prune;
    let mut report =
        super::ops_sql::execute_bounded_sweep(tx.connection(), tx.control(), &filtered, now_ms)?;
    report.settled.extend(local_settled);
    // Keep observable ordering identical to the original deterministic plan.
    report.settled.sort_by_key(|key| {
        plan.own_key_stale
            .iter()
            .chain(&plan.general_stale)
            .position(|k| k == key)
    });
    cleanup.retain(|job| report.settled.contains(&job.key) || report.pruned.contains(&job.key));
    enqueue_cleanup(tx, &cleanup)?;
    Ok(report)
}

fn settle_local_crash_in_tx(
    tx: &LedgerWriteTxn<'_>,
    job: &EmbeddingJob,
) -> Result<super::ops::TerminalReceipt> {
    if !job.binding.local || (job.known_result.is_some() && !job.key.is_device()) {
        return Err(invalid(
            "only an unknown local attempt can settle as local crash",
        ));
    }
    super::ops_sql::terminal_transaction(
        tx.connection(),
        tx.control(),
        &super::ops::TerminalWrite {
            key: &job.key,
            outcome: super::Outcome::UnknownSettled,
            billed: super::ops::nonbillable_charge(),
            ledger_batch_job_id: &job.intent_token,
            next_state: super::BatchState::Terminal,
            error: Some("local_crash"),
            increment_contract_violation: false,
            attempts_delta: 1,
            clear_intent_token: true,
            intent_token_guard: Some(&job.intent_token),
            reseat_submission_seq: false,
        },
    )
}

pub fn settle_local_embedding_crash(
    db: &LedgerDb,
    job: &EmbeddingJob,
) -> Result<super::ops::TerminalReceipt> {
    drain_cleanup(db)?;
    let receipt = db.write(|tx| {
        let row = super::ops_sql::get_batch_request(tx.connection(), &job.key)?
            .ok_or_else(|| invalid("local embedding reservation missing"))?;
        let current = load(tx.fs, tx.ledger_id, tx.era, &row, tx.connection())?
            .ok_or_else(|| invalid("local embedding binding missing"))?;
        if current.submission_seq != job.submission_seq || current.intent_token != job.intent_token
        {
            return Err(invalid("local embedding attempt changed"));
        }
        let receipt = settle_local_crash_in_tx(tx, &current)?;
        enqueue_cleanup(tx, &[current])?;
        Ok(receipt)
    })?;
    drain_cleanup(db)?;
    Ok(receipt)
}

pub(super) fn settle_unknown_with_cleanup(
    db: &LedgerDb,
    key: &TaskKey,
    intent_token: &str,
    estimated_usd: f64,
    cleanup_complete: bool,
) -> Result<super::ops::TerminalReceipt> {
    drain_cleanup(db)?;
    let receipt = db.write(|tx| {
        let original = if key.adapter_kind == "embedding" {
            if let Some(row) = super::ops_sql::get_batch_request(tx.connection(), key)?
                && row.request_kind == super::RequestKind::Sync
                && row.intent_token.as_deref() == Some(intent_token)
                && row.state.is_inflight()
            {
                load(tx.fs, tx.ledger_id, tx.era, &row, tx.connection())?
            } else {
                None
            }
        } else {
            None
        };
        if original
            .as_ref()
            .is_some_and(|job| job.known_result.is_some())
        {
            return Err(invalid(
                "a known embedding result must be recovered before unknown settlement",
            ));
        }
        let local = original.as_ref().is_some_and(|job| job.binding.local);
        let receipt = if let Some(job) = original.as_ref().filter(|job| job.binding.local) {
            settle_local_crash_in_tx(tx, job)?
        } else {
            super::ops_sql::recovery_settle_unknown(
                tx.connection(),
                tx.control(),
                key,
                intent_token,
                estimated_usd,
                cleanup_complete,
            )?
        };
        // Sync requests have no remote residue. Capture the original attempt
        // before reseating destroys its live-row locator; OCR/Batch contracts
        // continue to use their existing residue-cleanup state machine.
        if receipt.recorded
            && (cleanup_complete || local)
            && let Some(job) = original
        {
            enqueue_cleanup(tx, &[job])?;
        }
        Ok(receipt)
    })?;
    drain_cleanup(db)?;
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use super::super::{RequestKind, ops};
    use super::*;

    #[test]
    fn o1_committed_sweep_cleanup_is_rediscovered_after_process_loss() {
        let temp = tempfile::tempdir().unwrap();
        let db = LedgerDb::initialize(temp.path().join("device/cost-ledger.sqlite")).unwrap();
        let hash = |c: char| format!("sha256:{}", c.to_string().repeat(64));
        let key = TaskKey::new("scope_cleanup", "embedding", hash('a'), hash('b'));
        let binding = EmbeddingJobBinding {
            logical_job_id: EmbeddingLogicalJobId("logical_cleanup".to_owned()),
            local: true,
            dimensions: 3,
            distance: "cosine".to_owned(),
            modality: "multimodal".to_owned(),
            members: vec![EmbeddingJobMember {
                id: "member1".to_owned(),
                target: EmbeddingTaskTarget::Chunk {
                    chunk_hash: hash('c'),
                    embedding_hash: hash('a'),
                    profile_hash: hash('b'),
                    input_hash: hash('d'),
                },
            }],
        };
        let caps = ops::BudgetCapConfig {
            device_cap: 0.0,
            folder_cap: None,
            device_per_adapter_cap: None,
        };
        let request = EmbeddingReserveRequest {
            key: &key,
            candidate_usd: 0.0,
            caps: &caps,
            request_kind: RequestKind::Sync,
            timeout_seconds: Some(300),
            bypass_cap_denial: false,
            binding: &binding,
        };
        let EmbeddingReservationOutcome::Reserved(original) =
            reserve_embedding_job(&db, &request).unwrap()
        else {
            panic!("expected original reservation")
        };
        let rolled_back: Result<()> = db.write(|tx| {
            sweep_in_tx(
                tx,
                &ops::SweepPlan {
                    own_key_stale: vec![key.clone()],
                    general_stale: vec![],
                    prune: vec![],
                },
                db.now_millis(),
            )?;
            Err(invalid("intentional crash before sweep commit"))
        });
        assert!(rolled_back.is_err());
        let EmbeddingReservationOutcome::InFlight(still_live) =
            reserve_embedding_job(&db, &request).unwrap()
        else {
            panic!("uncommitted cleanup must preserve the original binding")
        };
        assert_eq!(still_live.intent_token, original.intent_token);
        assert!(ops::cost_ledger_rows_for_key(&db, &key).unwrap().is_empty());
        // Commit the sweep without running its post-commit cleanup,
        // matching process loss between SQLite commit and artifact deletion.
        db.write(|tx| {
            sweep_in_tx(
                tx,
                &ops::SweepPlan {
                    own_key_stale: vec![key.clone()],
                    general_stale: vec![],
                    prune: vec![],
                },
                db.now_millis(),
            )
        })
        .unwrap();
        let reopened = LedgerDb::open_existing(db.path()).unwrap();
        let EmbeddingReservationOutcome::Reserved(next) =
            reserve_embedding_job(&reopened, &request).unwrap()
        else {
            panic!("expected bounded next attempt")
        };
        assert_eq!(next.submission_seq, original.submission_seq + 1);
        let bindings = std::fs::read_dir(temp.path().join("device"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.to_string_lossy().ends_with(".binding.json"))
            .count();
        assert_eq!(
            bindings, 1,
            "committed cleanup must be rediscovered without a task or broad directory GC"
        );
    }

    #[test]
    fn o1_prepared_binding_orphan_allows_fresh_reservation_without_reusing_token() {
        let temp = tempfile::tempdir().unwrap();
        let db = LedgerDb::initialize(temp.path().join("device/cost-ledger.sqlite")).unwrap();
        let hash = |c: char| format!("sha256:{}", c.to_string().repeat(64));
        let key = TaskKey::new("scope_orphan", "embedding", hash('a'), hash('b'));
        let binding = EmbeddingJobBinding {
            logical_job_id: EmbeddingLogicalJobId("logical_orphan".to_owned()),
            local: true,
            dimensions: 3,
            distance: "cosine".to_owned(),
            modality: "multimodal".to_owned(),
            members: vec![EmbeddingJobMember {
                id: "member1".to_owned(),
                target: EmbeddingTaskTarget::Chunk {
                    chunk_hash: hash('c'),
                    embedding_hash: hash('a'),
                    profile_hash: hash('b'),
                    input_hash: hash('d'),
                },
            }],
        };
        let mut orphan = None;
        let failed: Result<()> = db.write(|tx| {
            super::super::ops_sql::phase1_intent(
                tx.connection(),
                tx.control(),
                &key,
                RequestKind::Sync,
                0.0,
                Some(300),
            )?;
            let row = super::super::ops_sql::get_batch_request(tx.connection(), &key)?.unwrap();
            orphan = Some(bind(tx, &row, &binding)?);
            Err(invalid("intentional failure before phase-1 commit"))
        });
        assert!(failed.is_err());
        assert!(ops::get_batch_request(&db, &key).unwrap().is_none());
        let orphan = orphan.unwrap();
        let fresh = reserve_embedding_job(
            &db,
            &EmbeddingReserveRequest {
                key: &key,
                candidate_usd: 0.0,
                caps: &ops::BudgetCapConfig {
                    device_cap: 0.0,
                    folder_cap: None,
                    device_per_adapter_cap: None,
                },
                request_kind: RequestKind::Sync,
                timeout_seconds: Some(300),
                bypass_cap_denial: false,
                binding: &binding,
            },
        )
        .expect("an uncommitted prepared binding must not block a fresh local attempt");
        let EmbeddingReservationOutcome::Reserved(fresh) = fresh else {
            panic!("expected a fresh orphan replacement")
        };
        assert_eq!(fresh.submission_seq, orphan.submission_seq);
        assert_ne!(fresh.intent_token, orphan.intent_token);
        assert!(fresh.known_result.is_none());
    }
}
