//! Device-private, explicit external-send grants.
//!
//! This store is deliberately separate from a scope's portable knowledge and
//! consent records.  Read-only callers never create a directory, lock leaf,
//! or repair an existing record.  Writers retain the capability-relative
//! directory and the lock file until their pending grant is activated or
//! revoked.

use std::path::{Path, PathBuf};

use kio_core::cas::canonical_json_bytes;
use kio_core::management::DirectoryIdentity;
use kio_core::scope::new_ulid;
use kio_core::store_dir::{
    ATOMIC_WORKSPACE_DIR, AtomicWorkspaceState, Publication, StoreDirectory,
};
use kio_core::{ExitCode, KioError, Result};
use serde::{Deserialize, Serialize};

const STORE_LEAF: &str = "grants.json";
const LOCK_LEAF: &str = ".lock";
const LOCK_BYTES: &[u8] = b"kio-private-grants-lock-v1\n";
const MAX_STORE_BYTES: u64 = 1024 * 1024;
const MAX_GRANTS: usize = 4096;
const SCHEMA_VERSION: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantOperation {
    Network,
    SendSecrets,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantState {
    Pending,
    Active,
    Revoked,
}

/// All fields are non-secret identity bindings.  In particular, credential
/// and trust fields are references/digests, never credential or certificate
/// bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantBinding {
    pub scope_id: String,
    pub canonical_root: PathBuf,
    pub directory_identity: DirectoryIdentity,
    pub membership_hash: String,
    pub tool_id: String,
    pub execution_mode: String,
    pub tool_profile_hash: String,
    pub destination: String,
    pub credential_binding: String,
    pub trust_binding: String,
    pub operation: GrantOperation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantRecord {
    pub schema_version: u8,
    pub grant_id: String,
    pub approval_id: String,
    pub device_instance_id: String,
    pub state: GrantState,
    pub binding: GrantBinding,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantSnapshot {
    records: Vec<GrantRecord>,
}

impl GrantSnapshot {
    #[must_use]
    pub fn records(&self) -> &[GrantRecord] {
        &self.records
    }
}

#[derive(Debug)]
pub struct PrivateGrantStore {
    state: crate::private_state::PrivateState,
    path: PathBuf,
    device_instance_id: String,
    records: Vec<GrantRecord>,
}

impl PrivateGrantStore {
    #[must_use]
    pub fn records(&self) -> &[GrantRecord] {
        &self.records
    }

    /// Open without creating or repairing anything.  `None` means the grant
    /// store directory or its record does not exist.
    pub fn open_readonly(path: &Path) -> Result<Option<GrantSnapshot>> {
        validate_store_path(path)?;
        let Some(parent) = path.parent() else {
            return Err(invalid("grant store path has no parent"));
        };
        let Some(directory) = crate::private_state::open_readonly(parent)? else {
            return Ok(None);
        };
        if validate_directory_entries(&directory)? == AtomicWorkspaceState::Pending {
            return Err(corrupt(
                "grant store atomic workspace requires writer recovery before read admission",
            ));
        }
        let Some(bytes) = directory.read_optional(Path::new(STORE_LEAF), MAX_STORE_BYTES)? else {
            return Ok(None);
        };
        // The retained capability read is the authority for ancestor, owner,
        // ACL, regular-file and single-link checks; the JSON read above is
        // only the bounded, descriptor-relative transport.
        let private_bytes =
            kio_core::private_fs::read_private_file_at(&directory, STORE_LEAF, MAX_STORE_BYTES)?;
        if private_bytes != bytes {
            return Err(corrupt("grant store changed while being read"));
        }
        let (_, records) = decode_store(&bytes)?;
        Ok(Some(GrantSnapshot { records }))
    }

    /// Create/open a private store and hold its retained lock for the caller's
    /// mutation session.  Existing permissions are checked, never repaired.
    pub fn open_or_create(path: &Path) -> Result<Self> {
        validate_store_path(path)?;
        let parent = path
            .parent()
            .ok_or_else(|| invalid("grant store path has no parent"))?;
        // Do a read-only inventory first. In particular, never create the
        // permanent lock inside a directory that already contains an unknown
        // sidecar; a known pending atomic workspace is recoverable after the
        // writer lease is held below.
        if let Some(directory) = crate::private_state::open_readonly(parent)? {
            validate_directory_entries(&directory)?;
        }
        let state = crate::private_state::open_or_create(parent, LOCK_BYTES)?;
        let directory = state.directory();
        if validate_directory_entries(directory)? != AtomicWorkspaceState::Absent {
            directory.recover_atomic(&[directory])?;
            if validate_directory_entries(directory)? == AtomicWorkspaceState::Pending {
                return Err(corrupt(
                    "grant store atomic workspace remained pending after writer recovery",
                ));
            }
        }
        let (device_instance_id, records) =
            match directory.read_optional(Path::new(STORE_LEAF), MAX_STORE_BYTES)? {
                Some(bytes) => {
                    let private_bytes = kio_core::private_fs::read_private_file_at(
                        directory,
                        STORE_LEAF,
                        MAX_STORE_BYTES,
                    )?;
                    if private_bytes != bytes {
                        return Err(corrupt("grant store changed while being read"));
                    }
                    decode_store(&bytes)?
                }
                None => (new_ulid(parent), Vec::new()),
            };
        if records
            .iter()
            .any(|record| record.device_instance_id != device_instance_id)
        {
            return Err(corrupt("grant records carry multiple device identities"));
        }
        Ok(Self {
            state,
            path: path.to_path_buf(),
            device_instance_id,
            records,
        })
    }

    pub fn begin_pending_approval(
        &mut self,
        binding: GrantBinding,
        send_secrets: bool,
        created_at: String,
    ) -> Result<(GrantRecord, Option<GrantRecord>)> {
        validate_timestamp(&created_at)?;
        if binding.operation != GrantOperation::Network {
            return Err(invalid("approval root binding must be a network grant"));
        }
        validate_binding(&binding)?;
        if self
            .records
            .iter()
            .any(|record| record.state == GrantState::Pending && record.binding == binding)
        {
            return Err(invalid("a matching approval intent is already pending"));
        }
        let approval_id = new_ulid(&self.path);
        if self.records.iter().any(|record| {
            record.approval_id == approval_id
                || (send_secrets
                    && record.state == GrantState::Pending
                    && record.approval_id == approval_id)
        }) {
            return Err(invalid("duplicate approval intent"));
        }
        let network = GrantRecord {
            schema_version: SCHEMA_VERSION,
            grant_id: new_ulid(&self.path),
            approval_id: approval_id.clone(),
            device_instance_id: self.device_instance_id.clone(),
            state: GrantState::Pending,
            binding: binding.clone(),
            created_at: created_at.clone(),
            activated_at: None,
            revoked_at: None,
        };
        let secret = if send_secrets {
            let mut secret_binding = binding;
            secret_binding.operation = GrantOperation::SendSecrets;
            Some(GrantRecord {
                schema_version: SCHEMA_VERSION,
                grant_id: new_ulid(&self.path),
                approval_id,
                device_instance_id: self.device_instance_id.clone(),
                state: GrantState::Pending,
                binding: secret_binding,
                created_at,
                activated_at: None,
                revoked_at: None,
            })
        } else {
            None
        };
        let old_len = self.records.len();
        self.records.push(network.clone());
        if let Some(record) = &secret {
            self.records.push(record.clone());
        }
        if let Err(error) = ensure_terminal_capacity(&self.records) {
            self.records.truncate(old_len);
            return Err(error);
        }
        if let Err(error) = self.persist(Publication::Upsert) {
            self.records.truncate(old_len);
            return Err(error);
        }
        Ok((network, secret))
    }

    pub fn activate_approval(
        &mut self,
        network: &GrantRecord,
        secret: Option<&GrantRecord>,
        activated_at: String,
    ) -> Result<()> {
        validate_timestamp(&activated_at)?;
        if network.binding.operation != GrantOperation::Network {
            return Err(invalid("approval network record has the wrong operation"));
        }
        let mut expected = vec![network];
        if let Some(secret) = secret {
            if secret.binding.operation != GrantOperation::SendSecrets
                || secret.approval_id != network.approval_id
            {
                return Err(invalid(
                    "approval secret record does not match network record",
                ));
            }
            expected.push(secret);
        }
        let indexes = expected
            .iter()
            .map(|expected| {
                self.records
                    .iter()
                    .position(|record| record.grant_id == expected.grant_id && record == *expected)
                    .ok_or_else(|| invalid("approval intent does not exist"))
            })
            .collect::<Result<Vec<_>>>()?;
        if indexes
            .iter()
            .any(|index| self.records[*index].state != GrantState::Pending)
        {
            return Err(invalid("only pending approval records can be activated"));
        }
        for index in &indexes {
            self.records[*index].state = GrantState::Active;
            self.records[*index].activated_at = Some(activated_at.clone());
        }
        if let Err(error) = self.persist(Publication::Replace) {
            for (index, expected) in indexes.iter().zip(expected) {
                self.records[*index] = expected.clone();
            }
            return Err(error);
        }
        Ok(())
    }

    pub fn revoke(
        &mut self,
        scope_id: &str,
        tool_id: Option<&str>,
        revoked_at: String,
    ) -> Result<Vec<String>> {
        self.revoke_matching(revoked_at, |binding| {
            binding.scope_id == scope_id
                && (tool_id.is_none() || Some(binding.tool_id.as_str()) == tool_id)
        })
    }

    /// Retire only grants issued to the removed physical scope instance. A
    /// separately re-registered copy with the same scope ID keeps its own
    /// authority; scope ID alone is not a device-grant retirement selector.
    pub fn revoke_scope_instance(
        &mut self,
        scope_id: &str,
        canonical_root: &Path,
        directory_identity: &DirectoryIdentity,
        revoked_at: String,
    ) -> Result<Vec<String>> {
        self.revoke_matching(revoked_at, |binding| {
            binding.scope_id == scope_id
                && binding.canonical_root == canonical_root
                && &binding.directory_identity == directory_identity
        })
    }

    fn revoke_matching(
        &mut self,
        revoked_at: String,
        matches_binding: impl Fn(&GrantBinding) -> bool,
    ) -> Result<Vec<String>> {
        validate_timestamp(&revoked_at)?;
        let before = self.records.clone();
        let mut changed = Vec::new();
        for record in &mut self.records {
            let matches = matches_binding(&record.binding) && record.state != GrantState::Revoked;
            if matches {
                record.state = GrantState::Revoked;
                record.revoked_at = Some(revoked_at.clone());
                changed.push(record.grant_id.clone());
            }
        }
        if let Err(error) = ensure_terminal_capacity(&self.records) {
            self.records = before;
            return Err(error);
        }
        if !changed.is_empty()
            && let Err(error) = self.persist(Publication::Replace)
        {
            self.records = before;
            return Err(error);
        }
        Ok(changed)
    }

    fn persist(&self, publication: Publication) -> Result<()> {
        ensure_terminal_capacity(&self.records)?;
        let value = serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "device_instance_id": &self.device_instance_id,
            "grants": &self.records,
        });
        let bytes = canonical_json_bytes(&value)?;
        if bytes.len() as u64 > MAX_STORE_BYTES {
            return Err(invalid("grant store exceeds its byte limit"));
        }
        self.state
            .directory()
            .write_atomic(Path::new(STORE_LEAF), &bytes, publication)?;
        self.state.directory().sync()
    }
}

fn validate_directory_entries(directory: &StoreDirectory) -> Result<AtomicWorkspaceState> {
    for entry in directory.entries(Path::new(""))? {
        let name = entry.name.to_string_lossy();
        if name == ATOMIC_WORKSPACE_DIR {
            if !entry.is_directory {
                return Err(corrupt("grant store atomic workspace is not a directory"));
            }
            continue;
        }
        if name != STORE_LEAF && name != LOCK_LEAF {
            return Err(corrupt("grant store contains an unexpected sidecar"));
        }
        if entry.is_directory || !entry.is_regular_file {
            return Err(corrupt("grant store contains a non-regular sidecar"));
        }
    }
    directory.inspect_atomic()
}

fn decode_store(bytes: &[u8]) -> Result<(String, Vec<GrantRecord>)> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|error| KioError::schema(error.to_string()))?;
    let canonical = canonical_json_bytes(&value)?;
    if canonical != bytes {
        return Err(corrupt("grant store is not canonical JSON"));
    }
    let object = value
        .as_object()
        .ok_or_else(|| invalid("grant store must be an object"))?;
    if object
        .keys()
        .any(|key| key != "schema_version" && key != "device_instance_id" && key != "grants")
    {
        return Err(corrupt("grant store contains an unknown field"));
    }
    let version = object
        .get("schema_version")
        .and_then(serde_json::Value::as_u64);
    if version != Some(SCHEMA_VERSION as u64) {
        return Err(corrupt("unsupported grant store schema"));
    }
    let device_instance_id = object
        .get("device_instance_id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| corrupt("grant store has no device identity"))?;
    let records: Vec<GrantRecord> = serde_json::from_value(
        value
            .get("grants")
            .cloned()
            .ok_or_else(|| invalid("grant store has no grants"))?,
    )
    .map_err(|error| KioError::schema(error.to_string()))?;
    if records.len() > MAX_GRANTS {
        return Err(invalid("grant store has too many grants"));
    }
    let mut ids = std::collections::BTreeSet::new();
    let mut approval_groups = std::collections::BTreeMap::<String, (usize, usize)>::new();
    for record in &records {
        let counts = approval_groups
            .entry(record.approval_id.clone())
            .or_default();
        match record.binding.operation {
            GrantOperation::Network => counts.0 += 1,
            GrantOperation::SendSecrets => counts.1 += 1,
        }
    }
    for (approval_id, (network_count, secret_count)) in &approval_groups {
        if *network_count != 1 || *secret_count > 1 {
            return Err(corrupt("approval group has duplicate or missing records"));
        }
        if let Some(secret) = records.iter().find(|record| {
            record.approval_id == *approval_id
                && record.binding.operation == GrantOperation::SendSecrets
        }) {
            let network = records
                .iter()
                .find(|record| {
                    record.approval_id == *approval_id
                        && record.binding.operation == GrantOperation::Network
                })
                .expect("network count was checked");
            if secret.state != network.state
                || secret.activated_at != network.activated_at
                || secret.revoked_at != network.revoked_at
            {
                return Err(corrupt("approval group states are inconsistent"));
            }
        }
    }
    for record in &records {
        validate_record(record)?;
        if record.schema_version != SCHEMA_VERSION || !ids.insert(record.grant_id.clone()) {
            return Err(corrupt("grant store has an invalid or duplicate grant"));
        }
        if record.approval_id.is_empty()
            || (record.approval_id == record.grant_id
                && record.binding.operation == GrantOperation::SendSecrets)
        {
            return Err(corrupt("grant record has an invalid approval identity"));
        }
        if record.device_instance_id != device_instance_id {
            return Err(corrupt("grant record device identity does not match store"));
        }
        if record.state == GrantState::Revoked && record.revoked_at.is_none() {
            return Err(corrupt("revoked grant has no revocation time"));
        }
        if record.state == GrantState::Active && record.activated_at.is_none() {
            return Err(corrupt("active grant has no activation time"));
        }
        if record.state == GrantState::Pending && record.activated_at.is_some() {
            return Err(corrupt("pending grant has an activation time"));
        }
        if record.state != GrantState::Revoked && record.revoked_at.is_some() {
            return Err(corrupt("non-revoked grant has a revocation time"));
        }
    }
    for record in &records {
        if record.binding.operation == GrantOperation::SendSecrets {
            let Some(network) = records.iter().find(|candidate| {
                candidate.approval_id == record.approval_id
                    && candidate.binding.operation == GrantOperation::Network
            }) else {
                return Err(corrupt("secret grant has no network approval group"));
            };
            let mut expected = record.binding.clone();
            expected.operation = GrantOperation::Network;
            if network.binding != expected {
                return Err(corrupt(
                    "secret grant binding differs from network approval",
                ));
            }
        }
    }
    ensure_terminal_capacity(&records)?;
    Ok((device_instance_id.to_owned(), records))
}

fn validate_timestamp(value: &str) -> Result<()> {
    let bytes = value.as_bytes();
    if bytes.len() != 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'Z'
        || bytes.iter().enumerate().any(|(index, byte)| {
            !matches!(index, 4 | 7 | 10 | 13 | 16 | 19) && !byte.is_ascii_digit()
        })
    {
        return Err(corrupt("timestamp is not fixed-width UTC"));
    }
    let year = value[0..4].parse::<u32>().unwrap_or(0);
    let month = value[5..7].parse::<u32>().unwrap_or(0);
    let day = value[8..10].parse::<u32>().unwrap_or(0);
    let hour = value[11..13].parse::<u32>().unwrap_or(u32::MAX);
    let minute = value[14..16].parse::<u32>().unwrap_or(u32::MAX);
    let second = value[17..19].parse::<u32>().unwrap_or(u32::MAX);
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => 0,
    };
    if year == 0 || day == 0 || day > days || hour > 23 || minute > 59 || second > 59 {
        return Err(corrupt("timestamp is not a valid UTC date"));
    }
    Ok(())
}

fn validate_record(record: &GrantRecord) -> Result<()> {
    if record.grant_id.is_empty()
        || record.approval_id.is_empty()
        || record.device_instance_id.is_empty()
    {
        return Err(corrupt("grant identity is empty"));
    }
    if !is_ulid(&record.grant_id)
        || !is_ulid(&record.approval_id)
        || !is_ulid(&record.device_instance_id)
    {
        return Err(corrupt("grant identity is not a canonical ULID"));
    }
    validate_binding(&record.binding)?;
    validate_timestamp(&record.created_at)?;
    if let Some(value) = &record.activated_at {
        validate_timestamp(value)?;
    }
    if let Some(value) = &record.revoked_at {
        validate_timestamp(value)?;
    }
    Ok(())
}

fn validate_binding(binding: &GrantBinding) -> Result<()> {
    if binding.scope_id.is_empty()
        || binding.tool_id.is_empty()
        || binding.execution_mode.is_empty()
        || binding.tool_profile_hash.is_empty()
        || binding.destination.is_empty()
        || binding.credential_binding.is_empty()
        || binding.trust_binding.is_empty()
    {
        return Err(corrupt("grant binding identity is empty"));
    }
    if !binding.canonical_root.is_absolute()
        || binding.canonical_root.components().any(|component| {
            matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
    {
        return Err(corrupt("grant binding root is not canonical absolute"));
    }
    Ok(())
}

fn is_ulid(value: &str) -> bool {
    value.len() == 26
        && value.bytes().all(|byte| {
            matches!(byte, b'0'..=b'9' | b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z')
        })
}

fn ensure_terminal_capacity(records: &[GrantRecord]) -> Result<()> {
    let mut terminal = records.to_vec();
    for record in &mut terminal {
        if record.state != GrantState::Revoked {
            record.state = GrantState::Revoked;
            if record.activated_at.is_none() {
                record.activated_at = Some("2000-01-01T00:00:00Z".to_owned());
            }
            record.revoked_at = Some("2000-01-01T00:00:00Z".to_owned());
        }
    }
    let value = serde_json::json!({
        "schema_version": SCHEMA_VERSION,
        "device_instance_id": terminal.first().map(|record| record.device_instance_id.as_str()).unwrap_or("device"),
        "grants": terminal,
    });
    if canonical_json_bytes(&value)?.len() as u64 > MAX_STORE_BYTES {
        return Err(invalid(
            "grant store lacks capacity for terminal revocation",
        ));
    }
    Ok(())
}

fn validate_store_path(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path.file_name() != Some(std::ffi::OsStr::new(STORE_LEAF))
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
    {
        return Err(invalid(
            "grant store path must be an absolute grants.json leaf",
        ));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> KioError {
    KioError::new(
        "KIO-E-GRANT-INVALID-001",
        message,
        serde_json::json!({}),
        ExitCode::InvalidUsage,
    )
}

fn corrupt(message: impl Into<String>) -> KioError {
    KioError::new(
        "KIO-E-GRANT-CORRUPT-001",
        message,
        serde_json::json!({}),
        ExitCode::PermanentFailure,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(root: &Path) -> GrantBinding {
        #[cfg(unix)]
        let directory_identity = DirectoryIdentity::Unix {
            device: 1,
            inode: 2,
        };
        #[cfg(windows)]
        let directory_identity = DirectoryIdentity::Windows {
            volume_serial_number: 1,
            file_index: 2,
        };
        GrantBinding {
            scope_id: "01J8KIO0000000000000000000".into(),
            canonical_root: root.to_path_buf(),
            directory_identity,
            membership_hash: "sha256:membership".into(),
            tool_id: "adapter-test".into(),
            execution_mode: "online_api".into(),
            tool_profile_hash: "sha256:profile".into(),
            destination: "https://127.0.0.1:9443".into(),
            credential_binding: "env:TEST_KEY".into(),
            trust_binding: "sha256:trust".into(),
            operation: GrantOperation::Network,
        }
    }

    #[test]
    fn writer_lifecycle_and_read_only_projection() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let path = root_path.join("nested/grants/grants.json");
        assert!(PrivateGrantStore::open_readonly(&path).unwrap().is_none());
        let mut store = PrivateGrantStore::open_or_create(&path).unwrap();
        let (pending, _) = store
            .begin_pending_approval(binding(&root_path), false, "2026-09-08T00:00:00Z".into())
            .unwrap();
        store
            .activate_approval(&pending, None, "2026-09-08T00:01:00Z".into())
            .unwrap();
        drop(store);
        let active = PrivateGrantStore::open_readonly(&path).unwrap().unwrap();
        assert_eq!(active.records().len(), 1);
        assert_eq!(
            active.records()[0].binding.scope_id,
            "01J8KIO0000000000000000000"
        );
        let directory =
            kio_core::private_fs::verify_private_directory(path.parent().unwrap()).unwrap();
        assert_eq!(
            directory.inspect_atomic().unwrap(),
            AtomicWorkspaceState::Clean,
            "atomic write workspace remains a validated known sidecar across reopen"
        );
        drop(active);
        let reopened = PrivateGrantStore::open_or_create(&path).unwrap();
        assert_eq!(reopened.records().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn readonly_rejects_pending_atomic_workspace_until_writer_recovers_without_losing_grants() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let path = root_path.join("grants/grants.json");
        let mut store = PrivateGrantStore::open_or_create(&path).unwrap();
        store
            .begin_pending_approval(binding(&root_path), false, "2026-09-08T00:00:00Z".into())
            .unwrap();
        drop(store);

        let directory =
            kio_core::private_fs::verify_private_directory(path.parent().unwrap()).unwrap();
        let workspace = directory.path().join(ATOMIC_WORKSPACE_DIR);
        assert_eq!(
            directory.inspect_atomic().unwrap(),
            AtomicWorkspaceState::Clean
        );
        let orphan = workspace.join("write");
        std::fs::write(&orphan, b"orphaned unpublished grant write").unwrap();
        std::fs::set_permissions(&orphan, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            directory.inspect_atomic().unwrap(),
            AtomicWorkspaceState::Pending
        );

        let grants_before = std::fs::read(&path).unwrap();
        let orphan_before = std::fs::read(&orphan).unwrap();
        assert!(PrivateGrantStore::open_readonly(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), grants_before);
        assert_eq!(std::fs::read(&orphan).unwrap(), orphan_before);

        let recovered = PrivateGrantStore::open_or_create(&path).unwrap();
        assert_eq!(recovered.records().len(), 1);
        drop(recovered);
        assert_eq!(std::fs::read(&path).unwrap(), grants_before);
        assert!(!orphan.exists());
        assert_eq!(
            directory.inspect_atomic().unwrap(),
            AtomicWorkspaceState::Clean
        );
        assert!(PrivateGrantStore::open_readonly(&path).unwrap().is_some());
    }

    #[test]
    fn revoke_is_terminal_and_idempotent() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let path = root_path.join("grants/grants.json");
        let mut store = PrivateGrantStore::open_or_create(&path).unwrap();
        let (pending, _) = store
            .begin_pending_approval(binding(&root_path), false, "2026-09-08T00:00:00Z".into())
            .unwrap();
        store
            .activate_approval(&pending, None, "2026-09-08T00:01:00Z".into())
            .unwrap();
        assert_eq!(
            store
                .revoke(
                    "01J8KIO0000000000000000000",
                    Some("adapter-test"),
                    "2026-09-08T01:00:00Z".into()
                )
                .unwrap(),
            vec![pending.grant_id.clone()]
        );
        assert!(
            store
                .revoke(
                    "01J8KIO0000000000000000000",
                    Some("adapter-test"),
                    "2026-09-08T02:00:00Z".into()
                )
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .activate_approval(&pending, None, "2026-09-08T03:00:00Z".into())
                .is_err()
        );
    }

    #[test]
    fn scope_instance_revocation_leaves_a_reregistered_same_id_active() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let replacement_path = root_path.join("replacement");
        std::fs::create_dir(&replacement_path).unwrap();
        let replacement_path = replacement_path.canonicalize().unwrap();
        let path = root_path.join("grants/grants.json");
        let mut store = PrivateGrantStore::open_or_create(&path).unwrap();

        let original_binding = binding(&root_path);
        let original_identity = original_binding.directory_identity.clone();
        let (original, _) = store
            .begin_pending_approval(original_binding, false, "2026-09-08T00:00:00Z".into())
            .unwrap();
        store
            .activate_approval(&original, None, "2026-09-08T00:01:00Z".into())
            .unwrap();

        let mut replacement_binding = binding(&replacement_path);
        replacement_binding.scope_id = original.binding.scope_id.clone();
        #[cfg(unix)]
        {
            replacement_binding.directory_identity = DirectoryIdentity::Unix {
                device: 3,
                inode: 4,
            };
        }
        #[cfg(windows)]
        {
            replacement_binding.directory_identity = DirectoryIdentity::Windows {
                volume_serial_number: 3,
                file_index: 4,
            };
        }
        let (replacement, _) = store
            .begin_pending_approval(replacement_binding, false, "2026-09-08T00:02:00Z".into())
            .unwrap();
        store
            .activate_approval(&replacement, None, "2026-09-08T00:03:00Z".into())
            .unwrap();

        assert_eq!(
            store
                .revoke_scope_instance(
                    &original.binding.scope_id,
                    &root_path,
                    &original_identity,
                    "2026-09-08T01:00:00Z".into(),
                )
                .unwrap(),
            vec![original.grant_id.clone()]
        );
        assert_eq!(
            store
                .records()
                .iter()
                .find(|record| record.grant_id == original.grant_id)
                .unwrap()
                .state,
            GrantState::Revoked
        );
        assert_eq!(
            store
                .records()
                .iter()
                .find(|record| record.grant_id == replacement.grant_id)
                .unwrap()
                .state,
            GrantState::Active
        );
        assert!(
            store
                .revoke_scope_instance(
                    &original.binding.scope_id,
                    &root_path,
                    &original_identity,
                    "2026-09-08T02:00:00Z".into(),
                )
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn approval_group_publishes_and_activates_atomically() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let path = root_path.join("grants/grants.json");
        let mut store = PrivateGrantStore::open_or_create(&path).unwrap();
        let (network, secret) = store
            .begin_pending_approval(binding(&root_path), true, "2026-09-08T00:00:00Z".into())
            .unwrap();
        let secret = secret.expect("secret approval was requested");
        assert_eq!(network.approval_id, secret.approval_id);
        assert_eq!(store.records().len(), 2);
        store
            .activate_approval(&network, Some(&secret), "2026-09-08T00:01:00Z".into())
            .unwrap();
        assert!(
            store
                .records()
                .iter()
                .all(|record| record.state == GrantState::Active)
        );
    }

    #[test]
    fn near_capacity_store_reserves_activation_and_revocation_room() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let path = root_path.join("grants/grants.json");
        let mut store = PrivateGrantStore::open_or_create(&path).unwrap();
        let payload = "x".repeat(9_000);
        let mut created = 0;
        for index in 0..256 {
            let mut grant_binding = binding(&root_path);
            grant_binding.tool_id = format!("adapter-{index}");
            grant_binding.destination = format!("https://example.invalid/{payload}");
            match store.begin_pending_approval(grant_binding, false, "2026-09-08T00:00:00Z".into())
            {
                Ok(_) => created += 1,
                Err(_) => break,
            }
        }
        assert!(
            created > 50,
            "capacity test did not approach the store limit"
        );
        assert!(created < 256, "capacity admission did not stop");
        let revoked = store
            .revoke(
                "01J8KIO0000000000000000000",
                None,
                "2026-09-08T01:00:00Z".into(),
            )
            .unwrap();
        assert_eq!(revoked.len(), created);
        assert!(
            store
                .records()
                .iter()
                .all(|record| record.state == GrantState::Revoked)
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_only_rejects_symlink_store_without_creating_sidecars() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let directory = root_path.join("grants");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(root_path.join("outside.json"), b"{}").unwrap();
        std::os::unix::fs::symlink(root_path.join("outside.json"), directory.join(STORE_LEAF))
            .unwrap();
        assert!(PrivateGrantStore::open_readonly(&directory.join(STORE_LEAF)).is_err());
        assert!(!directory.join(LOCK_LEAF).exists());
    }

    #[cfg(unix)]
    #[test]
    fn existing_group_writable_grant_directory_is_not_repaired() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let directory = root_path.join("grants");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o770)).unwrap();
        let path = directory.join(STORE_LEAF);
        assert!(PrivateGrantStore::open_or_create(&path).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn retained_lock_blocks_second_writer_and_releases_on_drop() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let path = root_path.join("grants/grants.json");
        let first = PrivateGrantStore::open_or_create(&path).unwrap();
        assert!(PrivateGrantStore::open_or_create(&path).is_err());
        drop(first);
        assert!(PrivateGrantStore::open_or_create(&path).is_ok());
    }
}
