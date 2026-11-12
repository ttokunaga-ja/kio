//! Durable, owner-private state for free local execution.
//!
//! A claim owns one permanent job lock until its caller has finished publishing.
//! The record is the attempt authority; a scope Task may mirror it but must not
//! spend an additional attempt after a crash between the two writes.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use kio_core::control::ControlHandle;
use kio_core::store_dir::{Publication, StoreDirectory};
use kio_core::{ExitCode, KioError, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::private_state::{self, PrivateState};
use kio_pipeline::task::EmbeddingTaskTarget;
use kio_pipeline::task::{RetryErrorKind, retry_policy};

const LOCK: &[u8] = b"kio.local-execution.lock/v1\n";
const RECORD: &str = "record.json";
const RESULT: &str = "result.bin";
const MAX_RECORD: u64 = 32 * 1024 * 1024;
const MAX_EMBEDDING_RESULT: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LocalExecutionKind {
    InProcess,
    Loopback,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum LocalTarget {
    Document {
        scope_id: String,
        target: String,
    },
    Image {
        scope_id: String,
        target: String,
    },
    Ocr {
        scope_id: String,
        target: String,
    },
    Query {
        query_sha256: String,
        query_kind: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LocalMember {
    pub id: String,
    pub target: EmbeddingTaskTarget,
}

/// All fields which affect result acceptance are part of the digest-named key.
/// For a query, `input_sha256` and `query_sha256` are digests, never query text.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LocalIdentity {
    pub target: LocalTarget,
    pub input_sha256: String,
    pub profile: String,
    pub execution: LocalExecutionKind,
    pub contract: LocalContract,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum LocalContract {
    Embedding {
        dimensions: u32,
        distance: String,
        modality: String,
        members: Vec<LocalMember>,
    },
    Ocr {
        raw_hash: String,
        tool_profile: String,
    },
}

impl LocalIdentity {
    fn validate(&self) -> Result<()> {
        if !is_digest(&self.input_sha256) || !bounded(&self.profile) {
            return Err(invalid("invalid local execution identity"));
        }
        match &self.contract {
            LocalContract::Embedding {
                dimensions,
                distance,
                modality,
                members,
            } if *dimensions > 0
                && *dimensions <= 65_536
                && bounded(distance)
                && bounded(modality)
                && !members.is_empty()
                && members.len() <= 4_096
                && members
                    .iter()
                    .all(|member| bounded(&member.id) && member.target.is_valid()) => {}
            LocalContract::Ocr {
                raw_hash,
                tool_profile,
            } if kio_core::cas::is_hash(raw_hash)
                && raw_hash.strip_prefix("sha256:") == Some(self.input_sha256.as_str())
                && bounded(tool_profile) => {}
            _ => return Err(invalid("invalid local execution contract")),
        }
        match &self.target {
            LocalTarget::Document { scope_id, target }
            | LocalTarget::Image { scope_id, target }
            | LocalTarget::Ocr { scope_id, target }
                if bounded(scope_id) && bounded(target) => {}
            LocalTarget::Query {
                query_sha256,
                query_kind,
            } if is_digest(query_sha256)
                && query_sha256 == &self.input_sha256
                && bounded(query_kind)
                && matches!(&self.contract, LocalContract::Embedding { members, .. }
                        if members.iter().all(|member| matches!(&member.target, EmbeddingTaskTarget::Query { .. }))) =>
                {}
            _ => return Err(invalid("invalid local execution target")),
        }
        if matches!(&self.target, LocalTarget::Ocr { .. })
            != matches!(&self.contract, LocalContract::Ocr { .. })
        {
            return Err(invalid("local execution target and contract differ"));
        }
        Ok(())
    }

    fn key(&self) -> Result<String> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|error| invalid(error.to_string()))?;
        Ok(hex_digest(&bytes))
    }
}

fn bounded(value: &str) -> bool {
    !value.is_empty() && value.len() <= 1_024 && !value.chars().any(char::is_control)
}

fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn invalid(message: impl Into<String>) -> KioError {
    KioError::invalid_usage(message.into())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
enum Phase {
    Intent,
    ResultStaging {
        digest: String,
        bytes: usize,
    },
    ResultReady {
        digest: String,
        bytes: usize,
    },
    Published {
        digest: String,
        bytes: usize,
    },
    Retiring {
        digest: String,
        bytes: usize,
    },
    Failed {
        kind: RetryErrorKind,
        next_retry_at: Option<i64>,
        terminal: bool,
    },
    Interrupted,
    Exhausted {
        kind: Option<RetryErrorKind>,
    },
    Unsent,
    Retired,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    key: String,
    identity: LocalIdentity,
    token: String,
    attempts: u32,
    phase: Phase,
}

impl Record {
    fn validate(&self, key: &str, identity: &LocalIdentity) -> Result<()> {
        if self.version != 1
            || self.key != key
            || &self.identity != identity
            || self.identity.key()? != key
            || !is_digest(&self.token)
            || (self.attempts == 0 && !matches!(self.phase, Phase::Failed { .. } | Phase::Unsent))
        {
            return Err(invalid("local execution record binding mismatch"));
        }
        if let Phase::ResultStaging { digest, bytes }
        | Phase::ResultReady { digest, bytes }
        | Phase::Published { digest, bytes }
        | Phase::Retiring { digest, bytes } = &self.phase
            && (!is_digest(digest)
                || *bytes == 0
                || (matches!(&self.identity.contract, LocalContract::Embedding { .. })
                    && *bytes > MAX_EMBEDDING_RESULT))
        {
            return Err(invalid("invalid local execution result locator"));
        }
        Ok(())
    }
}

pub(crate) struct LocalExecution {
    root: PathBuf,
    external_base: PathBuf,
    control: ControlHandle,
}

pub(crate) enum Claim {
    Claimed(Box<LocalGuard>),
    Busy,
    Backoff {
        attempts: u32,
        next_retry_at: i64,
        kind: Option<RetryErrorKind>,
    },
    Exhausted {
        attempts: u32,
        kind: Option<RetryErrorKind>,
    },
    // Document results remain claimable for idempotent scope publication.
}

pub(crate) struct LocalGuard {
    state: PrivateState,
    record: Record,
}

impl LocalExecution {
    pub(crate) fn new(
        root: PathBuf,
        external_base: PathBuf,
        control: ControlHandle,
    ) -> Result<Self> {
        if !root.is_absolute()
            || !external_base.is_absolute()
            || root
                .strip_prefix(&external_base)
                .ok()
                .is_none_or(|suffix| suffix.as_os_str().is_empty())
        {
            return Err(invalid(
                "local execution root must be below its absolute external base",
            ));
        }
        Ok(Self {
            root,
            external_base,
            control,
        })
    }

    pub(crate) fn default_external_base(control: &ControlHandle) -> Result<PathBuf> {
        kio_core::xdg::xdg_dir("XDG_STATE_HOME")
            .or_else(|| control.home_dir().map(|home| home.join(".local/state")))
            .ok_or_else(|| invalid("absolute state home is required"))
    }

    pub(crate) fn default_root(control: &ControlHandle) -> Result<PathBuf> {
        Ok(Self::default_external_base(control)?.join("kio/local-execution"))
    }

    pub(crate) fn claim(
        &self,
        identity: &LocalIdentity,
        floor_attempts: u32,
        floor_next_retry: Option<i64>,
        max_attempts: u32,
        now: i64,
    ) -> Result<Claim> {
        let key = identity.key()?;
        let max_attempts = match identity.execution {
            LocalExecutionKind::InProcess => max_attempts.min(3),
            LocalExecutionKind::Loopback => max_attempts,
        };
        if max_attempts == 0 {
            return Err(invalid("max_attempts must be positive"));
        }
        let path = self.root.join(&key);
        let existing =
            private_state::open_existing(&path, &self.external_base, LOCK, &self.control);
        let (state, mut record, created) = match existing {
            Ok(Some(state)) => {
                let record = read_record(state.directory(), &key, identity)?;
                (state, Some(record), false)
            }
            Ok(None) => {
                let initial = Record {
                    version: 1,
                    key: key.clone(),
                    identity: identity.clone(),
                    token: new_token()?,
                    attempts: floor_attempts.saturating_add(1),
                    phase: Phase::Intent,
                };
                if floor_attempts >= max_attempts {
                    return Ok(Claim::Exhausted {
                        attempts: floor_attempts,
                        kind: None,
                    });
                }
                if let Some(next_retry_at) = floor_next_retry.filter(|time| *time > now) {
                    return Ok(Claim::Backoff {
                        attempts: floor_attempts,
                        next_retry_at,
                        kind: None,
                    });
                }
                match private_state::create_initialized(
                    &path,
                    &self.external_base,
                    LOCK,
                    |directory| write_record(directory, &initial, Publication::CreateOnly),
                    &self.control,
                ) {
                    Ok(state) => (state, Some(initial), true),
                    Err(error) => match private_state::open_existing(
                        &path,
                        &self.external_base,
                        LOCK,
                        &self.control,
                    ) {
                        Ok(Some(state)) => {
                            let record = read_record(state.directory(), &key, identity)?;
                            (state, Some(record), false)
                        }
                        Err(lock_error)
                            if lock_error.error_code() == "KIO-E-PRIVATE-STATE-LOCKED-001" =>
                        {
                            return Ok(Claim::Busy);
                        }
                        _ => return Err(error),
                    },
                }
            }
            Err(error) if error.error_code() == "KIO-E-PRIVATE-STATE-LOCKED-001" => {
                return Ok(Claim::Busy);
            }
            Err(error) => return Err(error),
        };
        let mut record = record.take().expect("record constructed in each branch");
        let query = matches!(&identity.target, LocalTarget::Query { .. });
        match record.phase {
            Phase::Published { .. } if query => {
                start_retiring(state.directory(), &mut record)?;
            }
            Phase::Published { .. } | Phase::ResultReady { .. } => {
                verify_result(state.directory(), &record)?;
                return Ok(Claim::Claimed(Box::new(LocalGuard { state, record })));
            }
            Phase::ResultStaging { .. } => {
                state.directory().recover_atomic(&[state.directory()])?;
                if state.directory().contains_entry(Path::new(RESULT))? {
                    verify_result(state.directory(), &record)?;
                    let Phase::ResultStaging { digest, bytes } = &record.phase else {
                        unreachable!()
                    };
                    record.phase = Phase::ResultReady {
                        digest: digest.clone(),
                        bytes: *bytes,
                    };
                    write_record(state.directory(), &record, Publication::Replace)?;
                    return Ok(Claim::Claimed(Box::new(LocalGuard { state, record })));
                }
                // The result publication never completed. Its attempt remains
                // consumed and follows the usual interrupted-attempt policy.
                record.phase = Phase::Intent;
                write_record(state.directory(), &record, Publication::Replace)?;
            }
            Phase::Retiring { .. } => finish_retiring(state.directory(), &mut record)?,
            Phase::Intent
            | Phase::Failed { .. }
            | Phase::Interrupted
            | Phase::Exhausted { .. }
            | Phase::Unsent
            | Phase::Retired => {}
        }
        if state.directory().contains_entry(Path::new(RESULT))? {
            return Err(invalid("unbound local execution result must be preserved"));
        }
        if created {
            return Ok(Claim::Claimed(Box::new(LocalGuard { state, record })));
        }
        if record.attempts >= max_attempts
            && matches!(
                record.phase,
                Phase::Failed {
                    terminal: false,
                    ..
                } | Phase::Interrupted
            )
        {
            let (kind, deadline) = match record.phase {
                Phase::Failed {
                    kind,
                    next_retry_at,
                    ..
                } => (Some(kind), next_retry_at),
                Phase::Interrupted => (None, None),
                _ => unreachable!(),
            };
            if let Some(next_retry_at) = deadline.filter(|time| *time > now) {
                // A counted final attempt still owns its documented wait.
                // Neither its record nor token may change before that time.
                return Ok(Claim::Backoff {
                    attempts: record.attempts,
                    next_retry_at,
                    kind,
                });
            }
            record.phase = Phase::Exhausted { kind };
            write_record(state.directory(), &record, Publication::Replace)?;
            return Ok(Claim::Exhausted {
                attempts: record.attempts,
                kind,
            });
        }
        if query
            && matches!(
                record.phase,
                Phase::Failed { terminal: true, .. } | Phase::Exhausted { .. }
            )
        {
            archive_terminal(state.directory(), &record)?;
            record.attempts = 0;
            record.phase = Phase::Unsent;
            write_record(state.directory(), &record, Publication::Replace)?;
        }
        if matches!(record.phase, Phase::Intent) && record.attempts >= max_attempts {
            let kind = match identity.execution {
                LocalExecutionKind::InProcess => None,
                LocalExecutionKind::Loopback => Some(RetryErrorKind::NetworkError),
            };
            record.phase = Phase::Exhausted { kind };
            write_record(state.directory(), &record, Publication::Replace)?;
            return Ok(Claim::Exhausted {
                attempts: record.attempts,
                kind,
            });
        }
        if matches!(record.phase, Phase::Intent)
            && identity.execution == LocalExecutionKind::Loopback
        {
            // A dead owner consumed this attempt. Delay a resend even if the
            // Task mirror was never updated. Live owners cannot reach here.
            let now_secs = kio_core::scope::format_utc_seconds(now.div_euclid(1000));
            let policy = retry_policy(RetryErrorKind::NetworkError);
            let retry_at = crate::task_execution::scheduled_retry_at(
                &now_secs,
                &policy.backoff,
                record.attempts,
            );
            let retry_at = kio_core::scope::parse_utc_seconds(&retry_at)
                .map(|seconds| seconds.saturating_mul(1000))
                .unwrap_or(now.saturating_add(2_000));
            record.phase = Phase::Failed {
                kind: RetryErrorKind::NetworkError,
                next_retry_at: Some(retry_at),
                terminal: false,
            };
            write_record(state.directory(), &record, Publication::Replace)?;
        } else if matches!(record.phase, Phase::Intent) {
            record.phase = Phase::Interrupted;
            write_record(state.directory(), &record, Publication::Replace)?;
        }
        let retired = matches!(record.phase, Phase::Retired);
        if retired {
            if state.directory().contains_entry(Path::new(RESULT))? {
                return Err(invalid("retired local execution has an unexpected result"));
            }
            record.attempts = floor_attempts;
        }
        let kind = match record.phase {
            Phase::Failed { kind, .. } => Some(kind),
            Phase::Exhausted { kind } => kind,
            _ => None,
        };
        if matches!(record.phase, Phase::Failed { terminal: true, .. })
            || record.attempts >= max_attempts
        {
            return Ok(Claim::Exhausted {
                attempts: record.attempts,
                kind,
            });
        }
        let next_retry_at = match record.phase {
            Phase::Failed { next_retry_at, .. } => next_retry_at,
            _ => None,
        };
        let next_retry_at = if retired {
            next_retry_at.into_iter().chain(floor_next_retry).max()
        } else {
            next_retry_at
        };
        if let Some(next_retry_at) = next_retry_at.filter(|time| *time > now) {
            return Ok(Claim::Backoff {
                attempts: record.attempts,
                next_retry_at,
                kind,
            });
        }
        // New initialized records already contain their dispatch intent. An
        // existing one always rotates token and increments the attempt.
        record.attempts = record.attempts.saturating_add(1);
        record.token = new_token()?;
        record.phase = Phase::Intent;
        write_record(state.directory(), &record, Publication::Replace)?;
        Ok(Claim::Claimed(Box::new(LocalGuard { state, record })))
    }
}

impl LocalGuard {
    pub(crate) fn token(&self) -> &str {
        &self.record.token
    }
    pub(crate) fn attempts(&self) -> u32 {
        self.record.attempts
    }

    pub(crate) fn result(&self) -> Result<Option<Vec<u8>>> {
        self.check_current()?;
        match self.record.phase {
            Phase::ResultReady { .. } | Phase::Published { .. } => {
                verify_result(self.state.directory(), &self.record).map(Some)
            }
            _ => Ok(None),
        }
    }

    pub(crate) fn stage_result(&mut self, bytes: &[u8]) -> Result<()> {
        self.check_current()?;
        if !matches!(self.record.phase, Phase::Intent)
            || bytes.is_empty()
            || (matches!(
                &self.record.identity.contract,
                LocalContract::Embedding { .. }
            ) && bytes.len() > MAX_EMBEDDING_RESULT)
        {
            return Err(invalid(
                "local result cannot be staged in this state or size",
            ));
        }
        let directory = self.state.directory();
        if directory.contains_entry(Path::new(RESULT))? {
            return Err(invalid("unbound local execution result must be preserved"));
        }
        let digest = result_digest(&self.record, bytes);
        let mut staging = self.record.clone();
        staging.phase = Phase::ResultStaging {
            digest: digest.clone(),
            bytes: bytes.len(),
        };
        write_record(directory, &staging, Publication::Replace)?;
        self.record = staging;
        directory.write_atomic(Path::new(RESULT), bytes, Publication::CreateOnly)?;
        let mut ready = self.record.clone();
        ready.phase = Phase::ResultReady {
            digest,
            bytes: bytes.len(),
        };
        write_record(directory, &ready, Publication::Replace)?;
        self.record = ready;
        Ok(())
    }

    pub(crate) fn publish(&mut self) -> Result<()> {
        self.check_current()?;
        if matches!(self.record.phase, Phase::Published { .. }) {
            verify_result(self.state.directory(), &self.record)?;
            return Ok(());
        }
        if !matches!(self.record.phase, Phase::ResultReady { .. }) {
            return Err(invalid("local result is not ready for publication"));
        }
        verify_result(self.state.directory(), &self.record)?;
        let Phase::ResultReady { digest, bytes } = &self.record.phase else {
            unreachable!()
        };
        self.record.phase = Phase::Published {
            digest: digest.clone(),
            bytes: *bytes,
        };
        write_record(self.state.directory(), &self.record, Publication::Replace)
    }

    pub(crate) fn fail(
        &mut self,
        kind: RetryErrorKind,
        next_retry_at: Option<i64>,
        consume_attempt: bool,
        terminal: bool,
    ) -> Result<()> {
        self.check_current()?;
        if !matches!(self.record.phase, Phase::Intent) {
            return Err(invalid("only an active local attempt may fail"));
        }
        if !consume_attempt {
            // Rate-limit and pre-admission failures may preserve the retry
            // budget while the dispatch sequence remains durably recorded.
            self.record.attempts = self.record.attempts.saturating_sub(1);
        }
        self.record.phase = Phase::Failed {
            kind,
            next_retry_at,
            terminal,
        };
        write_record(self.state.directory(), &self.record, Publication::Replace)
    }

    pub(crate) fn interrupt(&mut self) -> Result<()> {
        self.check_current()?;
        if self.record.identity.execution != LocalExecutionKind::InProcess
            || !matches!(self.record.phase, Phase::Intent)
        {
            return Err(invalid("only an active in-process attempt may interrupt"));
        }
        self.record.phase = Phase::Interrupted;
        write_record(self.state.directory(), &self.record, Publication::Replace)
    }

    /// Refund a durable intent when admission failed before the adapter was
    /// invoked. The next claimant rotates the token before any dispatch.
    pub(crate) fn cancel_unsent(&mut self) -> Result<()> {
        self.check_current()?;
        if !matches!(self.record.phase, Phase::Intent) {
            return Err(invalid("only an unsent intent may be cancelled"));
        }
        self.record.attempts = self.record.attempts.saturating_sub(1);
        self.record.phase = Phase::Unsent;
        write_record(self.state.directory(), &self.record, Publication::Replace)
    }

    /// Removes only terminal artifacts; the coordination lock and directory
    /// stay forever so another claimant cannot lock a different inode.
    pub(crate) fn retire(&mut self) -> Result<()> {
        self.check_current()?;
        if !matches!(self.record.phase, Phase::Published { .. }) {
            return Err(invalid("only a published local result may retire"));
        }
        start_retiring(self.state.directory(), &mut self.record)
    }

    fn check_current(&self) -> Result<()> {
        let current = read_record(
            self.state.directory(),
            &self.record.key,
            &self.record.identity,
        )?;
        if current != self.record {
            return Err(invalid("stale local execution owner token or phase"));
        }
        Ok(())
    }
}

fn start_retiring(directory: &StoreDirectory, record: &mut Record) -> Result<()> {
    // A replaced Published leaf must remain untouched and diagnosable.
    verify_result(directory, record)?;
    let Phase::Published { digest, bytes } = &record.phase else {
        return Err(invalid("only a published local result may retire"));
    };
    let mut retiring = record.clone();
    retiring.phase = Phase::Retiring {
        digest: digest.clone(),
        bytes: *bytes,
    };
    write_record(directory, &retiring, Publication::Replace)?;
    *record = retiring;
    finish_retiring(directory, record)
}

fn archive_terminal(directory: &StoreDirectory, record: &Record) -> Result<()> {
    if !matches!(&record.identity.target, LocalTarget::Query { .. }) {
        return Err(invalid(
            "only a terminal query may start a fresh generation",
        ));
    }
    let leaf = format!("terminal-{}.json", record.token);
    let path = Path::new(&leaf);
    let bytes = serde_json::to_vec(record).map_err(|error| invalid(error.to_string()))?;
    if bytes.len() as u64 > MAX_RECORD {
        return Err(invalid("local terminal record too large"));
    }
    if let Some(existing) = directory.read_optional(path, MAX_RECORD)? {
        directory.ensure_owner_private(path)?;
        if existing != bytes {
            return Err(invalid(
                "local terminal archive differs from current attempt",
            ));
        }
    } else {
        directory.write_atomic(path, &bytes, Publication::CreateOnly)?;
    }
    Ok(())
}

fn finish_retiring(directory: &StoreDirectory, record: &mut Record) -> Result<()> {
    if !matches!(record.phase, Phase::Retiring { .. }) {
        return Err(invalid("local execution is not retiring"));
    }
    // The retained atomic workspace may hold an exact removal intent from a
    // crash. Recovery validates that intent and the original file identity.
    directory.recover_atomic(&[directory])?;
    if directory.contains_entry(Path::new(RESULT))? {
        let expected = verify_result(directory, record)?;
        directory.quarantine_then_remove(Path::new(RESULT), &expected, expected.len() as u64)?;
    }
    let mut retired = record.clone();
    retired.phase = Phase::Retired;
    write_record(directory, &retired, Publication::Replace)?;
    *record = retired;
    Ok(())
}

fn read_record(directory: &StoreDirectory, key: &str, identity: &LocalIdentity) -> Result<Record> {
    let bytes = directory
        .read_optional(Path::new(RECORD), MAX_RECORD)?
        .ok_or_else(|| invalid("local execution record is missing"))?;
    directory.ensure_owner_private(Path::new(RECORD))?;
    let record: Record = serde_json::from_slice(&bytes)
        .map_err(|error| invalid(format!("invalid local execution record: {error}")))?;
    record.validate(key, identity)?;
    Ok(record)
}

fn write_record(
    directory: &StoreDirectory,
    record: &Record,
    publication: Publication,
) -> Result<()> {
    let bytes = serde_json::to_vec(record).map_err(|error| invalid(error.to_string()))?;
    if bytes.len() as u64 > MAX_RECORD {
        return Err(invalid("local execution record too large"));
    }
    directory.write_atomic(Path::new(RECORD), &bytes, publication)
}

fn verify_result(directory: &StoreDirectory, record: &Record) -> Result<Vec<u8>> {
    let (digest, bytes) = match &record.phase {
        Phase::ResultStaging { digest, bytes }
        | Phase::ResultReady { digest, bytes }
        | Phase::Published { digest, bytes }
        | Phase::Retiring { digest, bytes } => (digest, bytes),
        _ => return Err(invalid("local result is not ready")),
    };
    directory.ensure_owner_private(Path::new(RESULT))?;
    let mut file = directory.open_regular_read(Path::new(RESULT), u64::MAX)?;
    let path = directory.path().join(RESULT);
    let actual_len = file
        .metadata()
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?
        .len();
    if actual_len != *bytes as u64 {
        return Err(invalid("local execution result length mismatch"));
    }
    // Check the retained file before allocating the full response. In
    // particular OCR may legitimately exceed the embedding artifact bound.
    let mut first_digest = result_hasher(record);
    read_digest_exact(&mut file, actual_len, &mut first_digest, &path)?;
    if hex_hasher(first_digest) != *digest {
        return Err(invalid("local execution result digest mismatch"));
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?;
    let mut result = Vec::new();
    result
        .try_reserve_exact(*bytes)
        .map_err(|_| resource_limit(*bytes))?;
    let mut second_digest = result_hasher(record);
    let mut remaining = *bytes;
    let mut buffer = [0_u8; 64 * 1024];
    while remaining > 0 {
        let count = remaining.min(buffer.len());
        file.read_exact(&mut buffer[..count])
            .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?;
        second_digest.update(&buffer[..count]);
        result.extend_from_slice(&buffer[..count]);
        remaining -= count;
    }
    if file
        .read(&mut buffer[..1])
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?
        != 0
        || hex_hasher(second_digest) != *digest
    {
        return Err(invalid("local execution result changed during read"));
    }
    Ok(result)
}

fn result_digest(record: &Record, bytes: &[u8]) -> String {
    let mut hasher = result_hasher(record);
    hasher.update(bytes);
    hex_hasher(hasher)
}

fn result_hasher(record: &Record) -> Sha256 {
    let mut hasher = Sha256::new();
    hasher.update(b"kio.local-execution.result/v1\0");
    hasher.update(record.key.as_bytes());
    hasher.update(record.token.as_bytes());
    hasher
}

fn hex_hasher(hasher: Sha256) -> String {
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn read_digest_exact(
    file: &mut std::fs::File,
    len: u64,
    hasher: &mut Sha256,
    path: &Path,
) -> Result<()> {
    let mut remaining = len;
    let mut buffer = [0_u8; 64 * 1024];
    while remaining > 0 {
        let count = usize::try_from(remaining.min(buffer.len() as u64)).expect("bounded by buffer");
        file.read_exact(&mut buffer[..count])
            .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?;
        hasher.update(&buffer[..count]);
        remaining -= count as u64;
    }
    if file
        .read(&mut buffer[..1])
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?
        != 0
    {
        return Err(invalid("local execution result grew during verification"));
    }
    Ok(())
}

fn resource_limit(observed: usize) -> KioError {
    KioError::new(
        "KIO-E-RESOURCE-LIMIT-001",
        "local result cannot fit in process memory",
        serde_json::json!({
            "resource": "memory",
            "limit": null,
            "observed": observed,
            "resource_tier": null,
            "retry_disposition": "explicit_after_resource_change",
        }),
        ExitCode::PartialFailure,
    )
}

fn new_token() -> Result<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| invalid(error.to_string()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query_identity() -> LocalIdentity {
        let digest = hex_digest(b"private query canary");
        LocalIdentity {
            target: LocalTarget::Query {
                query_sha256: digest.clone(),
                query_kind: "semantic".into(),
            },
            input_sha256: digest.clone(),
            profile: "profile".into(),
            execution: LocalExecutionKind::Loopback,
            contract: LocalContract::Embedding {
                dimensions: 3,
                distance: "cosine".into(),
                modality: "text".into(),
                members: vec![LocalMember {
                    id: "query".into(),
                    target: EmbeddingTaskTarget::Query {
                        query_hash: format!("sha256:{digest}"),
                        profile_hash: format!("sha256:{}", hex_digest(b"profile")),
                        input_hash: format!("sha256:{digest}"),
                    },
                }],
            },
        }
    }

    fn document_identity() -> LocalIdentity {
        let mut identity = query_identity();
        identity.target = LocalTarget::Document {
            scope_id: "scope".into(),
            target: format!("sha256:{}", hex_digest(b"document")),
        };
        let LocalContract::Embedding { members, .. } = &mut identity.contract else {
            unreachable!()
        };
        members[0].target = EmbeddingTaskTarget::Chunk {
            chunk_hash: format!("sha256:{}", hex_digest(b"chunk")),
            embedding_hash: format!("sha256:{}", hex_digest(b"document")),
            profile_hash: format!("sha256:{}", hex_digest(b"profile")),
            input_hash: format!("sha256:{}", identity.input_sha256),
        };
        identity
    }

    #[test]
    fn retirement_preserves_a_replaced_published_result() {
        let temp = tempfile::tempdir().unwrap();
        let store = LocalExecution::new(
            temp.path().join("state"),
            temp.path().to_path_buf(),
            ControlHandle::default(),
        )
        .unwrap();
        let identity = query_identity();
        let mut owner = match store.claim(&identity, 0, None, 5, 0).unwrap() {
            Claim::Claimed(owner) => owner,
            _ => panic!("initial claim must own the job"),
        };
        owner.stage_result(b"accepted result").unwrap();
        owner.publish().unwrap();
        owner
            .state
            .directory()
            .write_atomic(
                Path::new(RESULT),
                b"replacement is not the accepted result",
                Publication::Replace,
            )
            .unwrap();
        assert!(
            owner.retire().is_err(),
            "terminal cleanup must verify its exact published artifact"
        );
        assert_eq!(
            owner
                .state
                .directory()
                .read_optional(Path::new(RESULT), 1024)
                .unwrap()
                .unwrap(),
            b"replacement is not the accepted result",
            "unrecognized bytes must remain untouched"
        );
    }

    #[test]
    fn interrupted_retirement_rechecks_exact_result_before_cleanup() {
        let temp = tempfile::tempdir().unwrap();
        let store = LocalExecution::new(
            temp.path().join("state"),
            temp.path().to_path_buf(),
            ControlHandle::default(),
        )
        .unwrap();
        let identity = query_identity();
        let mut owner = match store.claim(&identity, 0, None, 5, 0).unwrap() {
            Claim::Claimed(owner) => owner,
            _ => panic!("initial claim must own the job"),
        };
        owner.stage_result(b"accepted result").unwrap();
        owner.publish().unwrap();
        let Phase::Published { digest, bytes } = &owner.record.phase else {
            panic!("published")
        };
        let mut retiring = owner.record.clone();
        retiring.phase = Phase::Retiring {
            digest: digest.clone(),
            bytes: *bytes,
        };
        write_record(owner.state.directory(), &retiring, Publication::Replace).unwrap();
        let path = owner.state.directory().path().to_path_buf();
        drop(owner);

        // A different payload appearing before recovery must remain untouched.
        std::fs::write(path.join(RESULT), b"foreign payload").unwrap();
        assert!(store.claim(&identity, 0, None, 5, 0).is_err());
        assert_eq!(
            std::fs::read(path.join(RESULT)).unwrap(),
            b"foreign payload"
        );
        std::fs::write(path.join(RESULT), b"accepted result").unwrap();
        let fresh = match store.claim(&identity, 0, None, 5, 0).unwrap() {
            Claim::Claimed(owner) => owner,
            _ => panic!("exact result retirement should resume to a fresh query"),
        };
        assert_eq!(fresh.attempts(), 1);
        assert!(!path.join(RESULT).exists());
    }

    #[test]
    fn retired_record_rejects_unexpected_leftover_result() {
        let temp = tempfile::tempdir().unwrap();
        let store = LocalExecution::new(
            temp.path().join("state"),
            temp.path().to_path_buf(),
            ControlHandle::default(),
        )
        .unwrap();
        let identity = query_identity();
        let mut owner = match store.claim(&identity, 0, None, 5, 0).unwrap() {
            Claim::Claimed(owner) => owner,
            _ => panic!("initial claim must own the job"),
        };
        owner.stage_result(b"accepted result").unwrap();
        owner.publish().unwrap();
        owner.retire().unwrap();
        let path = owner.state.directory().path().to_path_buf();
        owner
            .state
            .directory()
            .write_atomic(
                Path::new(RESULT),
                b"foreign payload",
                Publication::CreateOnly,
            )
            .unwrap();
        drop(owner);
        assert!(store.claim(&identity, 0, None, 5, 0).is_err());
        assert_eq!(
            std::fs::read(path.join(RESULT)).unwrap(),
            b"foreign payload"
        );
    }

    #[test]
    fn retirement_recovers_after_exact_remove_before_terminal_record() {
        let temp = tempfile::tempdir().unwrap();
        let store = LocalExecution::new(
            temp.path().join("state"),
            temp.path().to_path_buf(),
            ControlHandle::default(),
        )
        .unwrap();
        let identity = query_identity();
        let mut owner = match store.claim(&identity, 0, None, 5, 0).unwrap() {
            Claim::Claimed(owner) => owner,
            _ => panic!("initial claim must own the job"),
        };
        owner.stage_result(b"accepted result").unwrap();
        owner.publish().unwrap();
        let Phase::Published { digest, bytes } = &owner.record.phase else {
            panic!("published")
        };
        let mut retiring = owner.record.clone();
        retiring.phase = Phase::Retiring {
            digest: digest.clone(),
            bytes: *bytes,
        };
        write_record(owner.state.directory(), &retiring, Publication::Replace).unwrap();
        owner
            .state
            .directory()
            .quarantine_then_remove(
                Path::new(RESULT),
                b"accepted result",
                b"accepted result".len() as u64,
            )
            .unwrap();
        drop(owner);
        let fresh = match store.claim(&identity, 0, None, 5, 0).unwrap() {
            Claim::Claimed(owner) => owner,
            _ => panic!("missing result after exact remove should finish retirement"),
        };
        assert_eq!(fresh.attempts(), 1);
    }

    #[test]
    fn result_ready_recovers_without_spending_another_attempt_or_storing_query() {
        let temp = tempfile::tempdir().unwrap();
        let store = LocalExecution::new(
            temp.path().join("state"),
            temp.path().to_path_buf(),
            ControlHandle::default(),
        )
        .unwrap();
        let identity = query_identity();
        let mut owner = match store.claim(&identity, 0, None, 5, 0).unwrap() {
            Claim::Claimed(owner) => owner,
            _ => panic!("initial claim must own the job"),
        };
        assert!(matches!(
            store.claim(&identity, 0, None, 5, 0).unwrap(),
            Claim::Busy
        ));
        owner.stage_result(b"validated completion").unwrap();
        let first_token = owner.token().to_owned();
        drop(owner);

        let mut recovered = match store.claim(&identity, 0, None, 5, 0).unwrap() {
            Claim::Claimed(owner) => owner,
            _ => panic!("ready result must recover"),
        };
        assert_eq!(recovered.attempts(), 1);
        assert_eq!(recovered.token(), first_token);
        assert_eq!(
            recovered.result().unwrap().unwrap(),
            b"validated completion"
        );
        recovered.publish().unwrap();
        recovered.publish().unwrap();
        let record = std::fs::read(
            temp.path()
                .join("state")
                .join(identity.key().unwrap())
                .join(RECORD),
        )
        .unwrap();
        assert!(
            !record
                .windows(b"private query canary".len())
                .any(|w| w == b"private query canary")
        );
    }

    #[test]
    fn interrupted_intent_consumes_attempt_and_backoff_survives_restart() {
        let temp = tempfile::tempdir().unwrap();
        let store = LocalExecution::new(
            temp.path().join("state"),
            temp.path().to_path_buf(),
            ControlHandle::default(),
        )
        .unwrap();
        let identity = query_identity();
        let owner = match store.claim(&identity, 0, None, 5, 0).unwrap() {
            Claim::Claimed(owner) => owner,
            _ => panic!("initial claim must own the job"),
        };
        drop(owner);
        let retry_at = match store.claim(&identity, 0, None, 5, 0).unwrap() {
            Claim::Backoff {
                attempts: 1,
                next_retry_at,
                kind: Some(RetryErrorKind::NetworkError),
            } => next_retry_at,
            _ => panic!("interrupted attempt must back off"),
        };
        assert!(retry_at > 0);
        assert!(matches!(
            store.claim(&identity, 0, None, 5, retry_at - 1).unwrap(),
            Claim::Backoff { .. }
        ));
        let second = match store.claim(&identity, 0, None, 5, retry_at).unwrap() {
            Claim::Claimed(owner) => owner,
            _ => panic!("retry must be eligible after backoff"),
        };
        assert_eq!(second.attempts(), 2);
    }

    #[test]
    fn retired_query_is_a_fresh_invocation_with_the_same_lock() {
        let temp = tempfile::tempdir().unwrap();
        let store = LocalExecution::new(
            temp.path().join("state"),
            temp.path().to_path_buf(),
            ControlHandle::default(),
        )
        .unwrap();
        let identity = query_identity();
        for _ in 0..6 {
            let mut guard = match store.claim(&identity, 0, None, 5, 0).unwrap() {
                Claim::Claimed(guard) => guard,
                _ => panic!("a retired query must start a fresh invocation"),
            };
            assert_eq!(guard.attempts(), 1);
            guard.stage_result(b"validated completion").unwrap();
            guard.publish().unwrap();
            guard.retire().unwrap();
        }
    }

    #[test]
    fn published_query_from_previous_call_is_not_a_persistent_result_cache() {
        let temp = tempfile::tempdir().unwrap();
        let store = LocalExecution::new(
            temp.path().join("state"),
            temp.path().to_path_buf(),
            ControlHandle::default(),
        )
        .unwrap();
        let identity = query_identity();
        let mut first = match store.claim(&identity, 0, None, 5, 0).unwrap() {
            Claim::Claimed(guard) => guard,
            _ => panic!("initial query claim must own the job"),
        };
        let old_token = first.token().to_owned();
        first.stage_result(b"validated completion").unwrap();
        first.publish().unwrap();
        drop(first); // Crash before the caller's terminal retire.
        let next = match store.claim(&identity, 0, None, 5, 0).unwrap() {
            Claim::Claimed(guard) => guard,
            _ => panic!("a later query invocation must claim a fresh attempt"),
        };
        assert_ne!(next.token(), old_token);
        assert_eq!(next.attempts(), 1);
        assert!(next.result().unwrap().is_none());
    }

    #[test]
    fn terminal_query_failure_allows_later_fresh_call_but_live_backoff_holds() {
        let temp = tempfile::tempdir().unwrap();
        let store = LocalExecution::new(
            temp.path().join("state"),
            temp.path().to_path_buf(),
            ControlHandle::default(),
        )
        .unwrap();
        let identity = query_identity();
        let mut first = match store.claim(&identity, 0, None, 5, 0).unwrap() {
            Claim::Claimed(guard) => guard,
            _ => panic!("initial query claim must own the job"),
        };
        first
            .fail(RetryErrorKind::RateLimit, Some(10_000), false, false)
            .unwrap();
        drop(first);
        assert!(matches!(
            store.claim(&identity, 0, None, 5, 9_999).unwrap(),
            Claim::Backoff {
                attempts: 0,
                next_retry_at: 10_000,
                kind: Some(RetryErrorKind::RateLimit)
            }
        ));
        let mut retry = match store.claim(&identity, 0, None, 5, 10_000).unwrap() {
            Claim::Claimed(guard) => guard,
            _ => panic!("retry is eligible at its deadline"),
        };
        let old_token = retry.token().to_owned();
        retry
            .fail(RetryErrorKind::InvalidInput, None, true, true)
            .unwrap();
        drop(retry);
        let next = match store.claim(&identity, 0, None, 5, 10_000).unwrap() {
            Claim::Claimed(guard) => guard,
            _ => panic!("terminal failure belongs to the previous query call"),
        };
        assert_eq!(next.attempts(), 1);
        assert_ne!(next.token(), old_token);
    }

    #[test]
    fn query_attempt_limit_does_not_cancel_live_network_backoff() {
        let temp = tempfile::tempdir().unwrap();
        let store = LocalExecution::new(
            temp.path().join("state"),
            temp.path().to_path_buf(),
            ControlHandle::default(),
        )
        .unwrap();
        let identity = query_identity();
        let mut first = match store.claim(&identity, 0, None, 1, 0).unwrap() {
            Claim::Claimed(guard) => guard,
            _ => panic!("initial query claim must own the job"),
        };
        first
            .fail(RetryErrorKind::NetworkError, Some(10_000), true, false)
            .unwrap();
        let record_path = first.state.directory().path().join(RECORD);
        let before = std::fs::read(&record_path).unwrap();
        drop(first);
        assert!(matches!(
            store.claim(&identity, 0, None, 1, 9_999).unwrap(),
            Claim::Backoff {
                attempts: 1,
                next_retry_at: 10_000,
                kind: Some(RetryErrorKind::NetworkError)
            }
        ));
        assert_eq!(
            std::fs::read(record_path).unwrap(),
            before,
            "an unfinished backoff must retain its exact attempt record and token"
        );
        assert!(matches!(
            store.claim(&identity, 0, None, 1, 10_000).unwrap(),
            Claim::Exhausted {
                attempts: 1,
                kind: Some(RetryErrorKind::NetworkError)
            }
        ));
        let fresh = match store.claim(&identity, 0, None, 1, 10_000).unwrap() {
            Claim::Claimed(guard) => guard,
            _ => panic!("a later query call may start after terminal exhaustion was reported"),
        };
        assert_eq!(fresh.attempts(), 1);
    }

    #[test]
    fn unbound_result_under_intent_is_preserved_and_refused() {
        let temp = tempfile::tempdir().unwrap();
        let store = LocalExecution::new(
            temp.path().join("state"),
            temp.path().to_path_buf(),
            ControlHandle::default(),
        )
        .unwrap();
        let identity = query_identity();
        let owner = match store.claim(&identity, 0, None, 5, 0).unwrap() {
            Claim::Claimed(guard) => guard,
            _ => panic!("initial query claim must own the job"),
        };
        let path = owner.state.directory().path().join(RESULT);
        owner
            .state
            .directory()
            .write_atomic(
                Path::new(RESULT),
                b"unbound foreign result",
                Publication::CreateOnly,
            )
            .unwrap();
        drop(owner);
        assert!(store.claim(&identity, 0, None, 5, 0).is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"unbound foreign result");
    }

    #[test]
    fn staged_result_crash_recovers_exact_bytes_without_new_adapter_attempt() {
        let temp = tempfile::tempdir().unwrap();
        let store = LocalExecution::new(
            temp.path().join("state"),
            temp.path().to_path_buf(),
            ControlHandle::default(),
        )
        .unwrap();
        let identity = query_identity();
        let owner = match store.claim(&identity, 0, None, 5, 0).unwrap() {
            Claim::Claimed(guard) => guard,
            _ => panic!("initial claim must own the job"),
        };
        let mut staging = owner.record.clone();
        staging.phase = Phase::ResultStaging {
            digest: result_digest(&staging, b"accepted result"),
            bytes: b"accepted result".len(),
        };
        write_record(owner.state.directory(), &staging, Publication::Replace).unwrap();
        owner
            .state
            .directory()
            .write_atomic(
                Path::new(RESULT),
                b"accepted result",
                Publication::CreateOnly,
            )
            .unwrap();
        drop(owner);
        let recovered = match store.claim(&identity, 0, None, 5, 0).unwrap() {
            Claim::Claimed(guard) => guard,
            _ => panic!("exact staged result should recover"),
        };
        assert_eq!(recovered.attempts(), 1);
        assert_eq!(recovered.result().unwrap().unwrap(), b"accepted result");
    }

    #[test]
    fn staged_result_crash_rejects_mismatched_payload_without_removal() {
        let temp = tempfile::tempdir().unwrap();
        let store = LocalExecution::new(
            temp.path().join("state"),
            temp.path().to_path_buf(),
            ControlHandle::default(),
        )
        .unwrap();
        let identity = query_identity();
        let owner = match store.claim(&identity, 0, None, 5, 0).unwrap() {
            Claim::Claimed(guard) => guard,
            _ => panic!("initial claim must own the job"),
        };
        let mut staging = owner.record.clone();
        staging.phase = Phase::ResultStaging {
            digest: result_digest(&staging, b"accepted result"),
            bytes: b"accepted result".len(),
        };
        write_record(owner.state.directory(), &staging, Publication::Replace).unwrap();
        let path = owner.state.directory().path().join(RESULT);
        owner
            .state
            .directory()
            .write_atomic(
                Path::new(RESULT),
                b"foreign payload",
                Publication::CreateOnly,
            )
            .unwrap();
        drop(owner);
        assert!(store.claim(&identity, 0, None, 5, 0).is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"foreign payload");
    }

    #[test]
    fn unconsumed_rate_limit_and_terminal_error_survive_restart() {
        let temp = tempfile::tempdir().unwrap();
        let store = LocalExecution::new(
            temp.path().join("state"),
            temp.path().to_path_buf(),
            ControlHandle::default(),
        )
        .unwrap();
        let identity = document_identity();
        let mut first = match store.claim(&identity, 0, None, 5, 0).unwrap() {
            Claim::Claimed(guard) => guard,
            _ => panic!("initial claim must own the job"),
        };
        first
            .fail(RetryErrorKind::RateLimit, Some(10_000), false, false)
            .unwrap();
        drop(first);
        assert!(matches!(
            store.claim(&identity, 0, None, 5, 9_999).unwrap(),
            Claim::Backoff {
                attempts: 0,
                next_retry_at: 10_000,
                kind: Some(RetryErrorKind::RateLimit)
            }
        ));
        let mut second = match store.claim(&identity, 0, None, 5, 10_000).unwrap() {
            Claim::Claimed(guard) => guard,
            _ => panic!("rate limit should preserve the first counted attempt"),
        };
        assert_eq!(second.attempts(), 1);
        second
            .fail(RetryErrorKind::InvalidInput, None, true, true)
            .unwrap();
        drop(second);
        assert!(matches!(
            store.claim(&identity, 0, None, 5, 10_000).unwrap(),
            Claim::Exhausted {
                attempts: 1,
                kind: Some(RetryErrorKind::InvalidInput)
            }
        ));
    }

    #[test]
    fn durable_refund_overrides_stale_task_attempt_floor() {
        let temp = tempfile::tempdir().unwrap();
        let store = LocalExecution::new(
            temp.path().join("state"),
            temp.path().to_path_buf(),
            ControlHandle::default(),
        )
        .unwrap();
        let identity = query_identity();
        let mut first = match store.claim(&identity, 0, None, 5, 0).unwrap() {
            Claim::Claimed(guard) => guard,
            _ => panic!("initial claim must own the job"),
        };
        first
            .fail(RetryErrorKind::RateLimit, Some(10_000), false, false)
            .unwrap();
        drop(first);
        assert!(matches!(
            store.claim(&identity, 1, Some(20_000), 5, 9_999).unwrap(),
            Claim::Backoff {
                attempts: 0,
                next_retry_at: 10_000,
                kind: Some(RetryErrorKind::RateLimit)
            }
        ));
        let second = match store.claim(&identity, 1, Some(20_000), 5, 10_000).unwrap() {
            Claim::Claimed(guard) => guard,
            _ => panic!("durable refunded attempt and retry time must win"),
        };
        assert_eq!(second.attempts(), 1);
    }

    #[test]
    fn cancelled_unsent_intent_refunds_attempt_and_rotates_owner_token() {
        let temp = tempfile::tempdir().unwrap();
        let store = LocalExecution::new(
            temp.path().join("state"),
            temp.path().to_path_buf(),
            ControlHandle::default(),
        )
        .unwrap();
        let identity = query_identity();
        let mut first = match store.claim(&identity, 0, None, 5, 0).unwrap() {
            Claim::Claimed(guard) => guard,
            _ => panic!("initial claim must own the job"),
        };
        let token = first.token().to_owned();
        first.cancel_unsent().unwrap();
        drop(first);
        let second = match store.claim(&identity, 1, None, 5, 0).unwrap() {
            Claim::Claimed(guard) => guard,
            _ => panic!("unsent cancellation should allow another first attempt"),
        };
        assert_eq!(second.attempts(), 1);
        assert_ne!(second.token(), token);
    }

    #[test]
    fn corrupt_known_result_and_unknown_record_fields_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let store = LocalExecution::new(
            temp.path().join("state"),
            temp.path().to_path_buf(),
            ControlHandle::default(),
        )
        .unwrap();
        let identity = query_identity();
        let path = temp.path().join("state").join(identity.key().unwrap());
        let mut guard = match store.claim(&identity, 0, None, 5, 0).unwrap() {
            Claim::Claimed(guard) => guard,
            _ => panic!("initial claim must own the job"),
        };
        guard.stage_result(b"validated completion").unwrap();
        drop(guard);
        std::fs::write(path.join(RESULT), b"tampered completion").unwrap();
        assert!(store.claim(&identity, 0, None, 5, 0).is_err());
        let mut record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path.join(RECORD)).unwrap()).unwrap();
        record["unexpected"] = serde_json::json!(true);
        std::fs::write(path.join(RECORD), serde_json::to_vec(&record).unwrap()).unwrap();
        assert!(store.claim(&identity, 0, None, 5, 0).is_err());
        assert!(path.join(RECORD).exists());
    }

    #[test]
    fn missing_state_keeps_task_attempt_and_backoff_floor() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        let store = LocalExecution::new(
            root.clone(),
            temp.path().to_path_buf(),
            ControlHandle::default(),
        )
        .unwrap();
        let identity = query_identity();
        assert!(matches!(
            store.claim(&identity, 5, None, 5, 0).unwrap(),
            Claim::Exhausted {
                attempts: 5,
                kind: None
            }
        ));
        assert!(matches!(
            store.claim(&identity, 2, Some(10_000), 5, 0).unwrap(),
            Claim::Backoff {
                attempts: 2,
                next_retry_at: 10_000,
                kind: None
            }
        ));
        assert!(!root.join(identity.key().unwrap()).exists());
    }

    #[test]
    fn in_process_interruption_uses_three_logical_attempts_without_network_backoff() {
        let temp = tempfile::tempdir().unwrap();
        let store = LocalExecution::new(
            temp.path().join("state"),
            temp.path().to_path_buf(),
            ControlHandle::default(),
        )
        .unwrap();
        let mut identity = query_identity();
        identity.execution = LocalExecutionKind::InProcess;
        let first = match store.claim(&identity, 0, None, 5, 0).unwrap() {
            Claim::Claimed(guard) => guard,
            _ => panic!("initial claim must own the job"),
        };
        drop(first);
        let mut second = match store.claim(&identity, 0, None, 5, 0).unwrap() {
            Claim::Claimed(guard) => guard,
            _ => panic!("native crash can recompute immediately"),
        };
        assert_eq!(second.attempts(), 2);
        second.interrupt().unwrap();
        drop(second);
        let third = match store.claim(&identity, 0, None, 5, 0).unwrap() {
            Claim::Claimed(guard) => guard,
            _ => panic!("native interruption can recompute within budget"),
        };
        assert_eq!(third.attempts(), 3);
        drop(third);
        assert!(matches!(
            store.claim(&identity, 0, None, 5, 0).unwrap(),
            Claim::Exhausted {
                attempts: 3,
                kind: None
            }
        ));
    }
}
