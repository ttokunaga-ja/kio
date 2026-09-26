//! Explicit device-local CA trust lifecycle for offline adapters.
//!
//! The active record contains only a generation, state, digest, and managed
//! snapshot name.  Certificate bytes are written once to an owner-private
//! snapshot before its record is published; an interrupted rotation therefore
//! leaves either the prior active record or a fully durable new snapshot.

use std::path::{Path, PathBuf};

#[cfg(test)]
use kio_core::cas::hash_bytes;
use kio_core::store_dir::{Publication, StoreDirectory};
use kio_core::{ExitCode, KioError, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

const ACTIVE: &str = "active.json";
const PENDING: &str = "pending-registration.json";
const LOCK_BYTES: &[u8] = b"kio-local-trust-lock-v1\n";
const MAX_CA_BYTES: u64 = 1024 * 1024;
const MAX_RECORD_BYTES: u64 = 16 * 1024;
const RECORD_VERSION: u8 = 1;

/// A source is reduced to protected bytes once.  Its digest is the exact
/// DER-normalized trust identity the adapter will later enforce for the
/// immutable managed snapshot.
struct ValidatedSource {
    bytes: Vec<u8>,
    digest: String,
}

#[cfg(test)]
thread_local! {
    static TEST_DIRECTORY: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TrustStatus {
    Active,
    Revoked,
}

/// Device-local state only. It intentionally never serializes certificate
/// material or a source path supplied by the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TrustRecord {
    version: u8,
    generation: u64,
    status: TrustStatus,
    digest: String,
    snapshot: String,
    registration_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingRegistration {
    version: u8,
    registration_id: String,
    generation: u64,
    digest: String,
    snapshot: String,
    predecessor: Option<TrustRecord>,
}

/// A retained lock held from active-record validation until the caller's
/// adapter dispatch returns. It serializes trust lifecycle changes with the
/// send-time authority window.
pub(crate) struct RuntimeTrust {
    _state: crate::private_state::PrivateState,
    path: PathBuf,
    digest: String,
    binding: String,
}

impl RuntimeTrust {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
    pub(crate) fn digest(&self) -> &str {
        &self.digest
    }
    pub(crate) fn binding(&self) -> &str {
        &self.binding
    }
}

fn directory_path() -> PathBuf {
    #[cfg(test)]
    if let Some(path) = TEST_DIRECTORY.with(|slot| slot.borrow().clone()) {
        return path;
    }
    super::data_home().join("kio/local-trust")
}

/// Read status without creating a directory, lock, repair, or snapshot.
#[cfg(test)]
pub(crate) fn status() -> Result<Option<TrustRecord>> {
    read_record()
}

/// Acquire the active managed CA and retain the trust-store lock until the
/// returned guard is dropped. This prevents a revoke or rotation from racing a
/// command after its egress authority has been admitted.
pub(crate) fn acquire_runtime_trust() -> Result<Option<RuntimeTrust>> {
    let Some(state) = crate::private_state::open_existing(&directory_path(), LOCK_BYTES)? else {
        return Ok(None);
    };
    let record = read_record_from(state.directory())?.ok_or_else(missing_record)?;
    if let Some(pending) = read_pending_from(state.directory())? {
        return Err(pending_requires_resume(&pending));
    }
    if record.status != TrustStatus::Active {
        return Ok(None);
    }
    let path = directory_path().join(&record.snapshot);
    let digest = kio_adapter::local_peer::capture_private_local_trust(&path)
        .map_err(super::adapter_to_kio)?;
    if digest != record.digest {
        return Err(err(
            "KIO-E-LOCAL-TRUST-CHANGED-001",
            "managed local CA snapshot no longer matches its active record",
        ));
    }
    Ok(Some(RuntimeTrust {
        _state: state,
        path,
        digest,
        binding: format!(
            "local-ca:v{}:g{}:{}:{}",
            record.version, record.generation, record.registration_id, record.digest
        ),
    }))
}

/// Register a managed local CA. The first record starts at generation one;
/// a durable revocation may be explicitly registered again only as a new,
/// monotonic generation. An active record is never replaced by register.
pub(crate) fn register(ca_pem: &Path, preview: bool, resume: Option<&str>) -> Result<TrustRecord> {
    let source = validated_source(ca_pem)?;
    if preview {
        return preview_registration(&source, resume);
    }
    if crate::private_state::open_readonly(&directory_path())?.is_none() {
        if resume.is_some() {
            return Err(absent());
        }
        let record = register_record_for_digest(None, &source.digest, true)?;
        let state =
            crate::private_state::create_initialized(&directory_path(), LOCK_BYTES, |directory| {
                write_pending(directory, &record, None)
            })?;
        publish(
            state.directory(),
            &record,
            &source.bytes,
            Publication::CreateOnly,
        )?;
        clear_pending(state.directory())?;
        return Ok(record);
    }
    let state =
        crate::private_state::open_existing(&directory_path(), LOCK_BYTES)?.ok_or_else(absent)?;
    let current = read_record_from(state.directory())?;
    let pending = read_pending_from(state.directory())?;
    if let Some(resume) = resume {
        return resume_registration(state.directory(), current, pending, &source, resume);
    }
    if let Some(pending) = pending.as_ref() {
        return Err(pending_requires_resume(pending));
    }
    let record = register_record_for_digest(current.as_ref().cloned(), &source.digest, false)?;
    write_pending(state.directory(), &record, current.clone())?;
    let publication = if current.is_some() {
        Publication::Replace
    } else {
        Publication::CreateOnly
    };
    publish(state.directory(), &record, &source.bytes, publication)?;
    clear_pending(state.directory())?;
    Ok(record)
}

fn preview_registration(source: &ValidatedSource, resume: Option<&str>) -> Result<TrustRecord> {
    let Some(directory) = crate::private_state::open_readonly(&directory_path())? else {
        if resume.is_some() {
            return Err(absent());
        }
        return register_record_for_digest(None, &source.digest, true);
    };
    let current = read_record_from(&directory)?;
    let pending = read_pending_from(&directory)?;
    if let Some(resume) = resume {
        return resume_record(current, pending, &source.digest, resume);
    }
    if let Some(pending) = pending {
        return Err(pending_requires_resume(&pending));
    }
    register_record_for_digest(current, &source.digest, false)
}

fn resume_registration(
    directory: &StoreDirectory,
    current: Option<TrustRecord>,
    pending: Option<PendingRegistration>,
    source: &ValidatedSource,
    resume: &str,
) -> Result<TrustRecord> {
    let pending = pending.ok_or_else(resume_conflict)?;
    let record = resume_record(
        current.clone(),
        Some(pending.clone()),
        &source.digest,
        resume,
    )?;
    match current {
        Some(active) if active == record => {
            clear_pending(directory)?;
            return Ok(active);
        }
        Some(active) if pending.predecessor.as_ref() == Some(&active) => {
            publish(directory, &record, &source.bytes, Publication::Replace)?;
        }
        None if pending.predecessor.is_none() => {
            publish(directory, &record, &source.bytes, Publication::CreateOnly)?;
        }
        _ => return Err(resume_conflict()),
    }
    clear_pending(directory)?;
    Ok(record)
}

fn resume_record(
    current: Option<TrustRecord>,
    pending: Option<PendingRegistration>,
    digest: &str,
    resume: &str,
) -> Result<TrustRecord> {
    let pending = pending.ok_or_else(resume_conflict)?;
    if pending.registration_id != resume || pending.digest != digest {
        return Err(resume_conflict());
    }
    let record = TrustRecord {
        version: pending.version,
        generation: pending.generation,
        status: TrustStatus::Active,
        digest: pending.digest,
        snapshot: pending.snapshot,
        registration_id: pending.registration_id,
    };
    let _ = current;
    Ok(record)
}

/// Rotate an active managed CA. A rotation assigns a new monotonic generation,
/// so grants bound to the prior digest no longer match.
pub(crate) fn rotate(ca_pem: &Path, preview: bool) -> Result<TrustRecord> {
    let source = validated_source(ca_pem)?;
    if preview {
        let current = active_record(read_record()?)?;
        return rotated_record_for_digest(&current, &source.digest);
    }

    let Some(state) = crate::private_state::open_existing(&directory_path(), LOCK_BYTES)? else {
        return Err(absent());
    };
    if let Some(pending) = read_pending_from(state.directory())? {
        return Err(pending_requires_resume(&pending));
    }
    let current = active_record(read_record_from(state.directory())?)?;
    let record = rotated_record_for_digest(&current, &source.digest)?;
    publish(
        state.directory(),
        &record,
        &source.bytes,
        Publication::Replace,
    )?;
    Ok(record)
}

/// Persist a revocation tombstone. Repeating a completed revoke is idempotent;
/// status and a missing record remain read-only.
pub(crate) fn revoke(preview: bool) -> Result<Option<TrustRecord>> {
    if preview {
        let Some(initial) = read_record()? else {
            return Ok(None);
        };
        let Some(directory) = crate::private_state::open_readonly(&directory_path())? else {
            return Ok(None);
        };
        if let Some(pending) = read_pending_from(&directory)? {
            return Err(pending_requires_resume(&pending));
        }
        return Ok(Some(if initial.status == TrustStatus::Revoked {
            initial
        } else {
            TrustRecord {
                status: TrustStatus::Revoked,
                ..initial
            }
        }));
    }
    let Some(state) = crate::private_state::open_existing(&directory_path(), LOCK_BYTES)? else {
        return Ok(None);
    };
    if let Some(pending) = read_pending_from(state.directory())? {
        return Err(pending_requires_resume(&pending));
    }
    let record = read_record_from(state.directory())?.ok_or_else(missing_record)?;
    if record.status == TrustStatus::Revoked {
        return Ok(Some(record));
    }
    let revoked = TrustRecord {
        status: TrustStatus::Revoked,
        ..record
    };
    write_active(state.directory(), &revoked, Publication::Replace)?;
    Ok(Some(revoked))
}

#[cfg(test)]
fn register_record(
    current: Option<TrustRecord>,
    bytes: &[u8],
    fresh_store: bool,
) -> Result<TrustRecord> {
    register_record_for_digest(current, &hash_bytes(bytes), fresh_store)
}

/// Construct a record from the adapter's canonical certificate identity.
/// Runtime registration must use this form so its persisted binding matches
/// `AuthenticatedLocalEndpoint` rather than the PEM serialization bytes.
fn register_record_for_digest(
    current: Option<TrustRecord>,
    digest: &str,
    fresh_store: bool,
) -> Result<TrustRecord> {
    match current {
        None if fresh_store => Ok(record_for_digest(1, digest, new_registration_id())),
        None => Err(err(
            "KIO-E-LOCAL-TRUST-RECORD-MISSING-001",
            "managed local trust store has no active record; refusing to reinitialize it",
        )),
        Some(record) if record.status == TrustStatus::Active => Err(already_registered()),
        Some(record) => {
            let generation = record.generation.checked_add(1).ok_or_else(|| {
                err(
                    "KIO-E-LOCAL-TRUST-OVERFLOW-001",
                    "local CA generation overflow",
                )
            })?;
            Ok(record_for_digest(generation, digest, new_registration_id()))
        }
    }
}

fn active_record(record: Option<TrustRecord>) -> Result<TrustRecord> {
    let record = record.ok_or_else(absent)?;
    if record.status != TrustStatus::Active {
        return Err(err(
            "KIO-E-LOCAL-TRUST-REVOKED-001",
            "revoked local CA trust must be registered anew",
        ));
    }
    Ok(record)
}

#[cfg(test)]
fn rotated_record(current: &TrustRecord, bytes: &[u8]) -> Result<TrustRecord> {
    rotated_record_for_digest(current, &hash_bytes(bytes))
}

fn rotated_record_for_digest(current: &TrustRecord, digest: &str) -> Result<TrustRecord> {
    if digest == current.digest {
        return Err(err(
            "KIO-E-LOCAL-TRUST-UNCHANGED-001",
            "rotated local CA must differ from the active generation",
        ));
    }
    let generation = current.generation.checked_add(1).ok_or_else(|| {
        err(
            "KIO-E-LOCAL-TRUST-OVERFLOW-001",
            "local CA generation overflow",
        )
    })?;
    Ok(record_for_digest(generation, digest, new_registration_id()))
}

#[cfg(test)]
fn record_for(generation: u64, bytes: &[u8], registration_id: String) -> TrustRecord {
    record_for_digest(generation, &hash_bytes(bytes), registration_id)
}

fn record_for_digest(generation: u64, digest: &str, registration_id: String) -> TrustRecord {
    TrustRecord {
        version: RECORD_VERSION,
        generation,
        status: TrustStatus::Active,
        digest: digest.to_owned(),
        snapshot: snapshot_name_from_digest(digest),
        registration_id,
    }
}

fn new_registration_id() -> String {
    kio_core::scope::new_ulid(&directory_path())
}

fn valid_registration_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
}

fn pending_for(record: &TrustRecord, predecessor: Option<TrustRecord>) -> PendingRegistration {
    PendingRegistration {
        version: RECORD_VERSION,
        registration_id: record.registration_id.clone(),
        generation: record.generation,
        digest: record.digest.clone(),
        snapshot: record.snapshot.clone(),
        predecessor,
    }
}

fn validate_pending(pending: &PendingRegistration) -> Result<()> {
    let record = TrustRecord {
        version: pending.version,
        generation: pending.generation,
        status: TrustStatus::Active,
        digest: pending.digest.clone(),
        snapshot: pending.snapshot.clone(),
        registration_id: pending.registration_id.clone(),
    };
    validate_record(&record)?;
    match &pending.predecessor {
        None if pending.generation == 1 => Ok(()),
        Some(predecessor)
            if predecessor.status == TrustStatus::Revoked
                && predecessor.generation.checked_add(1) == Some(pending.generation)
                && predecessor.registration_id != pending.registration_id =>
        {
            validate_record(predecessor)
        }
        _ => Err(err(
            "KIO-E-LOCAL-TRUST-PENDING-CORRUPT-001",
            "trust registration intent has an invalid predecessor binding",
        )),
    }
}

fn read_pending_from(directory: &StoreDirectory) -> Result<Option<PendingRegistration>> {
    let Some(bytes) = directory.read_optional(Path::new(PENDING), MAX_RECORD_BYTES)? else {
        return Ok(None);
    };
    if kio_core::private_fs::read_private_file_at(directory, PENDING, MAX_RECORD_BYTES)? != bytes {
        return Err(err(
            "KIO-E-LOCAL-TRUST-PENDING-CHANGED-001",
            "trust registration intent changed while being read",
        ));
    }
    let pending: PendingRegistration =
        serde_json::from_slice(&bytes).map_err(|error| KioError::schema(error.to_string()))?;
    validate_pending(&pending)?;
    Ok(Some(pending))
}

fn write_pending(
    directory: &StoreDirectory,
    record: &TrustRecord,
    predecessor: Option<TrustRecord>,
) -> Result<()> {
    let bytes = serde_json::to_vec(&pending_for(record, predecessor))
        .map_err(|error| KioError::schema(error.to_string()))?;
    directory.write_atomic(Path::new(PENDING), &bytes, Publication::CreateOnly)?;
    directory.ensure_owner_private(Path::new(PENDING))?;
    directory.sync()
}

fn clear_pending(directory: &StoreDirectory) -> Result<()> {
    if directory
        .read_optional(Path::new(PENDING), MAX_RECORD_BYTES)?
        .is_some()
    {
        directory.remove_file(Path::new(PENDING))?;
        directory.sync()?;
    }
    Ok(())
}

fn read_record() -> Result<Option<TrustRecord>> {
    let Some(directory) = crate::private_state::open_readonly(&directory_path())? else {
        return Ok(None);
    };
    read_record_from(&directory)?
        .ok_or_else(missing_record)
        .map(Some)
}

/// Descriptor-relative bytes establish the retained directory authority. The
/// second no-follow private read validates owner/ACL and detects record changes
/// without reimporting the retained directory's diagnostic pathname.
fn read_record_from(directory: &StoreDirectory) -> Result<Option<TrustRecord>> {
    let Some(bytes) = directory.read_optional(Path::new(ACTIVE), MAX_RECORD_BYTES)? else {
        return Ok(None);
    };
    let private_bytes =
        kio_core::private_fs::read_private_file_at(directory, ACTIVE, MAX_RECORD_BYTES)?;
    if private_bytes != bytes {
        return Err(err(
            "KIO-E-LOCAL-TRUST-CHANGED-002",
            "local CA trust record changed while being read",
        ));
    }
    let record: TrustRecord =
        serde_json::from_slice(&bytes).map_err(|error| KioError::schema(error.to_string()))?;
    validate_record(&record)?;
    Ok(Some(record))
}

fn validate_record(record: &TrustRecord) -> Result<()> {
    if record.version != RECORD_VERSION
        || record.generation == 0
        || !record.digest.starts_with("sha256:")
        || record.digest.len() != "sha256:".len() + 64
        || !record
            .digest
            .bytes()
            .skip("sha256:".len())
            .all(|byte| byte.is_ascii_hexdigit())
        || !record.snapshot.starts_with("ca-")
        || !record.snapshot.ends_with(".pem")
        || record.snapshot != snapshot_name_from_digest(&record.digest)
        || !valid_registration_id(&record.registration_id)
    {
        return Err(err(
            "KIO-E-LOCAL-TRUST-CORRUPT-001",
            "local CA trust record is invalid",
        ));
    }
    Ok(())
}

fn publish(
    directory: &StoreDirectory,
    record: &TrustRecord,
    bytes: &[u8],
    active_publication: Publication,
) -> Result<()> {
    // The immutable object always reaches durable storage before an active
    // record can name it. `CreateOnly` also rejects a substituted collision.
    let snapshot = Path::new(&record.snapshot);
    if let Err(write_error) = directory.write_atomic(snapshot, bytes, Publication::CreateOnly) {
        // Returning to an earlier CA must allocate a new active generation but
        // reuse its immutable snapshot. PEM wrapping is not an authority
        // change: the DER-normalized digest in the record is the identity the
        // TLS endpoint enforces. Any other parsed identity remains a hard
        // collision; retained bytes are never replaced.
        match directory.read_optional(snapshot, MAX_CA_BYTES)? {
            Some(existing) => {
                let existing_digest =
                    kio_adapter::local_peer::local_trust_digest_from_pem_bytes(&existing)
                        .map_err(super::adapter_to_kio)?;
                if existing_digest != record.digest {
                    return Err(write_error);
                }
            }
            None => return Err(write_error),
        }
    }
    directory.ensure_owner_private(snapshot)?;
    directory.sync()?;
    write_active(directory, record, active_publication)
}

fn write_active(
    directory: &StoreDirectory,
    record: &TrustRecord,
    publication: Publication,
) -> Result<()> {
    let bytes = serde_json::to_vec(record).map_err(|error| KioError::schema(error.to_string()))?;
    directory.write_atomic(Path::new(ACTIVE), &bytes, publication)?;
    directory.ensure_owner_private(Path::new(ACTIVE))?;
    directory.sync()
}

fn validated_source(path: &Path) -> Result<ValidatedSource> {
    if !path.is_absolute() {
        return Err(KioError::invalid_usage("--ca-pem must be an absolute path"));
    }
    let bytes = kio_core::private_fs::read_private_file(path, MAX_CA_BYTES)?;
    let digest = kio_adapter::local_peer::local_trust_digest_from_pem_bytes(&bytes)
        .map_err(super::adapter_to_kio)?;
    // Do not reopen `path` here: a second pathname read creates a TOCTOU
    // window. The owner-private no-follow read above is the complete source
    // authority; these retained bytes are what gets published.
    Ok(ValidatedSource { bytes, digest })
}
fn snapshot_name_from_digest(digest: &str) -> String {
    format!("ca-{}.pem", digest.trim_start_matches("sha256:"))
}
fn pending_requires_resume(pending: &PendingRegistration) -> KioError {
    KioError::new(
        "KIO-E-LOCAL-TRUST-REGISTRATION-PENDING-001",
        "local trust registration is pending; resume it with its registration ID",
        json!({"registration_id": pending.registration_id}),
        ExitCode::PermanentFailure,
    )
}
fn resume_conflict() -> KioError {
    err(
        "KIO-E-LOCAL-TRUST-RESUME-CONFLICT-001",
        "registration resume does not match the current pending trust intent",
    )
}
fn missing_record() -> KioError {
    err(
        "KIO-E-LOCAL-TRUST-RECORD-MISSING-001",
        "managed local trust store has no active record; refusing to reinitialize it",
    )
}
fn absent() -> KioError {
    err(
        "KIO-E-LOCAL-TRUST-ABSENT-001",
        "local CA trust is not registered",
    )
}
fn already_registered() -> KioError {
    err(
        "KIO-E-LOCAL-TRUST-EXISTS-001",
        "local CA trust is already registered; use rotate or revoke",
    )
}
fn err(code: &str, message: &str) -> KioError {
    KioError::new(code, message, json!({}), ExitCode::PermanentFailure)
}

/// Execute the scope-independent CLI lifecycle. Values intentionally expose
/// only state, generation, and digest; certificate bytes and source paths are
/// never rendered.
pub(crate) fn run(command: crate::commands::TrustCommand) -> Result<serde_json::Value> {
    use crate::commands::TrustCommand;

    match command {
        TrustCommand::Register(args) => {
            validate_confirmation(args.preview, args.yes)?;
            let record = register(&args.ca_pem, args.preview, args.resume.as_deref())?;
            Ok(record_value(
                if args.preview {
                    "preview"
                } else {
                    "registered"
                },
                Some(record),
            ))
        }
        TrustCommand::Rotate(args) => {
            validate_confirmation(args.preview, args.yes)?;
            let record = rotate(&args.ca_pem, args.preview)?;
            Ok(record_value(
                if args.preview { "preview" } else { "rotated" },
                Some(record),
            ))
        }
        TrustCommand::Revoke => Ok(record_value("revoked", revoke(false)?)),
        TrustCommand::Status => status_value(),
    }
}

fn validate_confirmation(preview: bool, yes: bool) -> Result<()> {
    if preview == yes {
        return Err(KioError::invalid_usage(
            "exactly one of --preview or --yes is required for trust register or rotate",
        ));
    }
    Ok(())
}

fn status_value() -> Result<serde_json::Value> {
    let Some(directory) = crate::private_state::open_readonly(&directory_path())? else {
        return Ok(record_value("status", None));
    };
    let record = read_record_from(&directory)?;
    let pending = read_pending_from(&directory)?;
    match (record, pending) {
        (_, Some(pending)) => Ok(json!({
            "operation": "status",
            "trust": "registration_pending",
            "registration_id": pending.registration_id,
        })),
        (Some(record), None) => Ok(record_value("status", Some(record))),
        (None, None) => Err(missing_record()),
    }
}

fn record_value(operation: &str, record: Option<TrustRecord>) -> serde_json::Value {
    match record {
        Some(record) => json!({ "operation": operation, "trust": record }),
        None => json!({ "operation": operation, "trust": "unregistered" }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct TestDirectory(Option<PathBuf>);
    impl TestDirectory {
        fn install(path: PathBuf) -> Self {
            let prior = TEST_DIRECTORY.with(|slot| slot.replace(Some(path)));
            Self(prior)
        }
    }
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            TEST_DIRECTORY.with(|slot| {
                slot.replace(self.0.take());
            });
        }
    }

    #[test]
    fn record_accepts_only_its_own_immutable_snapshot_name() {
        let bytes = b"public-ca-material";
        let record = record_for(7, bytes, "test-registration".to_owned());
        assert!(validate_record(&record).is_ok());

        let mut mismatched = record;
        mismatched.snapshot =
            "ca-0000000000000000000000000000000000000000000000000000000000000000.pem".into();
        assert!(validate_record(&mismatched).is_err());
    }

    #[test]
    fn lifecycle_generation_changes_when_a_ca_returns_after_rotation_or_revoke() {
        let ca_a = b"ca-a";
        let ca_b = b"ca-b";
        let first = record_for(1, ca_a, "registration-a".to_owned());
        let second = rotated_record(&first, ca_b).unwrap();
        let returned = rotated_record(&second, ca_a).unwrap();
        assert_eq!(returned.generation, 3);
        assert_eq!(returned.digest, first.digest);
        assert_ne!(
            format!(
                "local-ca:v{}:g{}:{}",
                first.version, first.generation, first.digest
            ),
            format!(
                "local-ca:v{}:g{}:{}",
                returned.version, returned.generation, returned.digest
            ),
        );

        let revoked = TrustRecord {
            status: TrustStatus::Revoked,
            ..returned
        };
        assert_eq!(
            register_record(Some(revoked), ca_a, false)
                .unwrap()
                .generation,
            4
        );
    }

    #[test]
    fn resume_replaces_only_its_exact_revoked_predecessor() {
        let predecessor = TrustRecord {
            status: TrustStatus::Revoked,
            ..record_for(1, b"ca-a", "registration-a".to_owned())
        };
        let desired = record_for(2, b"ca-b", "registration-b".to_owned());
        let pending = pending_for(&desired, Some(predecessor.clone()));
        assert!(validate_pending(&pending).is_ok());
        assert_eq!(
            resume_record(
                Some(predecessor),
                Some(pending),
                &hash_bytes(b"ca-b"),
                "registration-b"
            )
            .unwrap(),
            desired
        );
    }

    #[test]
    fn resume_requires_the_exact_durable_intent_and_ca_digest() {
        let bytes = b"ca-a";
        let record = record_for(1, bytes, "registration-a".to_owned());
        let pending = pending_for(&record, None);
        assert_eq!(
            resume_record(
                None,
                Some(pending.clone()),
                &hash_bytes(bytes),
                "registration-a"
            )
            .unwrap(),
            record
        );
        assert!(
            resume_record(None, Some(pending), &hash_bytes(b"ca-b"), "registration-a").is_err()
        );
    }

    #[test]
    fn missing_active_record_cannot_reactivate_a_prior_ca_generation() {
        let ca_a = b"ca-a";
        let ca_b = b"ca-b";
        let first = record_for(1, ca_a, "registration-a".to_owned());
        let _second = rotated_record(&first, ca_b).unwrap();
        let error = register_record(None, ca_a, false).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("KIO-E-LOCAL-TRUST-RECORD-MISSING-001")
        );
    }

    #[test]
    fn status_of_absent_store_creates_no_state() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let _directory = TestDirectory::install(root.join("kio/local-trust"));
        assert_eq!(status().unwrap(), None);
        assert!(!root.join("kio").exists());
    }

    #[cfg(unix)]
    #[test]
    fn reregistering_equivalent_pem_reuses_snapshot_with_a_fresh_generation() {
        use std::os::unix::fs::PermissionsExt;

        // The newline changes the source bytes but not the parsed DER trust
        // anchor. This is the exact collision case for canonical snapshots.
        const PEM: &str = "-----BEGIN CERTIFICATE-----\nMIIBWTCB/6ADAgECAhR5P0J0YMFaZPlrOFyN8/jwpGzhqjAKBggqhkjOPQQDAjAh\nMR8wHQYDVQQDDBZyY2dlbiBzZWxmIHNpZ25lZCBjZXJ0MCAXDTc1MDEwMTAwMDAw\nMFoYDzQwOTYwMTAxMDAwMDAwWjAhMR8wHQYDVQQDDBZyY2dlbiBzZWxmIHNpZ25l\nZCBjZXJ0MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEGhIwEPQnvWSlH+iUQ5Ui\nq0khmBj4hlxmW+mTL5xjjyxwwopOnkxLs2AofasS2lYPdILzMaIeRE78g2S7Hz1n\nX6MTMBEwDwYDVR0RBAgwBocEfwAAATAKBggqhkjOPQQDAgNJADBGAiEA0GBG4uEL\nSJJea18R17+yhRTyn3rsD6PzDhdg7F/v2sQCIQCccgcgjDh+ABNajeb1deKZoqbx\nnP4qBrGe09azOI4jbg==\n-----END CERTIFICATE-----\n";
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let _directory = TestDirectory::install(root.join("kio/local-trust"));
        let first_source = root.join("first.pem");
        let equivalent_source = root.join("equivalent.pem");
        std::fs::write(&first_source, PEM).unwrap();
        std::fs::write(&equivalent_source, format!("{PEM}\n")).unwrap();
        for path in [&first_source, &equivalent_source] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        let first = register(&first_source, false, None).unwrap();
        revoke(false).unwrap();
        let returned = register(&equivalent_source, false, None).unwrap();

        assert_eq!(returned.digest, first.digest);
        assert_eq!(returned.snapshot, first.snapshot);
        assert_eq!(returned.generation, first.generation + 1);
        assert_ne!(returned.registration_id, first.registration_id);
        assert_eq!(status().unwrap(), Some(returned));
    }
}
