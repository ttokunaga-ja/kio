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
pub(crate) const LOCK_BYTES: &[u8] = b"kio-private-grants-lock-v1\n";
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
pub struct ScopedGrantBinding {
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
#[serde(untagged)]
pub enum GrantBinding {
    Scoped(ScopedGrantBinding),
    DeviceEndpoint(DeviceEndpointBinding),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceEndpointBinding {
    pub destination: String,
    pub credential_binding: String,
    pub trust_binding: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointDisclosure {
    pub tool_id: String,
    pub tool_profile_hash: String,
    pub purpose: String,
    pub content_types: Vec<String>,
    pub applies_to: String,
    pub peer_authentication: String,
    pub send_secrets: bool,
}

impl GrantBinding {
    pub fn operation(&self) -> GrantOperation {
        match self {
            Self::Scoped(binding) => binding.operation,
            Self::DeviceEndpoint(_) => GrantOperation::Network,
        }
    }
    pub fn scoped(&self) -> Option<&ScopedGrantBinding> {
        match self {
            Self::Scoped(binding) => Some(binding),
            _ => None,
        }
    }
}

mod endpoint;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantRecord {
    pub schema_version: u8,
    pub grant_id: String,
    pub approval_id: String,
    pub device_instance_id: String,
    pub state: GrantState,
    pub binding: GrantBinding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disclosure: Option<EndpointDisclosure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_network_grant_id: Option<String>,
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
    pub fn open_readonly(
        path: &Path,
        external_base: &Path,
        control: &kio_core::control::ControlHandle,
    ) -> Result<Option<GrantSnapshot>> {
        validate_store_path(path)?;
        let Some(parent) = path.parent() else {
            return Err(invalid("grant store path has no parent"));
        };
        let Some(directory) = crate::private_state::open_readonly(parent, external_base, control)?
        else {
            return Ok(None);
        };
        Self::read_snapshot_at(&directory)
    }

    /// Read only through the retained directory protected by the caller's
    /// lease. Its diagnostic pathname is never reopened for grant authority.
    pub(crate) fn read_snapshot_at(directory: &StoreDirectory) -> Result<Option<GrantSnapshot>> {
        kio_core::private_fs::verify_private_directory_handle(directory)?;
        if validate_directory_entries(directory)? == AtomicWorkspaceState::Pending {
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
        kio_core::private_fs::verify_private_directory_handle(directory)?;
        let private_bytes =
            kio_core::private_fs::read_private_file_at(directory, STORE_LEAF, MAX_STORE_BYTES)?;
        if private_bytes != bytes {
            return Err(corrupt("grant store changed while being read"));
        }
        let (_, records) = decode_store(&bytes)?;
        Ok(Some(GrantSnapshot { records }))
    }

    /// Create/open a private store and hold its retained lock for the caller's
    /// mutation session.  Existing permissions are checked, never repaired.
    pub fn open_or_create(
        path: &Path,
        external_base: &Path,
        control: &kio_core::control::ControlHandle,
    ) -> Result<Self> {
        validate_store_path(path)?;
        let parent = path
            .parent()
            .ok_or_else(|| invalid("grant store path has no parent"))?;
        // Do a read-only inventory first. In particular, never create the
        // permanent lock inside a directory that already contains an unknown
        // sidecar; a known pending atomic workspace is recoverable after the
        // writer lease is held below.
        if let Some(directory) =
            crate::private_state::open_readonly(parent, external_base, control)?
        {
            validate_directory_entries(&directory)?;
        }
        let state =
            crate::private_state::open_or_create(parent, external_base, LOCK_BYTES, control)?;
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
                    kio_core::private_fs::verify_private_directory_handle(directory)?;
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
        binding: ScopedGrantBinding,
        send_secrets: bool,
        created_at: String,
    ) -> Result<(GrantRecord, Option<GrantRecord>)> {
        validate_timestamp(&created_at)?;
        if binding.operation != GrantOperation::Network {
            return Err(invalid("approval root binding must be a network grant"));
        }
        validate_binding(&binding)?;
        if self.records.iter().any(|record| {
            record.state == GrantState::Pending
                && record.binding == GrantBinding::Scoped(binding.clone())
        }) {
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
            binding: GrantBinding::Scoped(binding.clone()),
            disclosure: None,
            parent_network_grant_id: None,
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
                binding: GrantBinding::Scoped(secret_binding),
                disclosure: None,
                parent_network_grant_id: None,
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
        if network.binding.operation() != GrantOperation::Network {
            return Err(invalid("approval network record has the wrong operation"));
        }
        let mut expected = vec![network];
        if let Some(secret) = secret {
            if secret.binding.operation() != GrantOperation::SendSecrets
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
            binding.scoped().is_some_and(|binding| {
                binding.scope_id == scope_id
                    && (tool_id.is_none() || Some(binding.tool_id.as_str()) == tool_id)
            })
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
            binding.scoped().is_some_and(|binding| {
                binding.scope_id == scope_id
                    && binding.canonical_root == canonical_root
                    && &binding.directory_identity == directory_identity
            })
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
    validate_records(device_instance_id, &records)?;
    ensure_terminal_capacity(&records)?;
    Ok((device_instance_id.to_owned(), records))
}

fn validate_records(device: &str, records: &[GrantRecord]) -> Result<()> {
    if records.len() > MAX_GRANTS || !is_ulid(device) {
        return Err(corrupt("invalid device grant inventory"));
    }
    let mut ids = std::collections::BTreeSet::new();
    for record in records {
        validate_record(record)?;
        if record.schema_version != SCHEMA_VERSION
            || record.device_instance_id != device
            || !ids.insert(&record.grant_id)
        {
            return Err(corrupt("invalid or duplicate grant identity"));
        }
        if (record.state == GrantState::Active && record.activated_at.is_none())
            || (record.state == GrantState::Pending && record.activated_at.is_some())
            || (record.state == GrantState::Revoked) != record.revoked_at.is_some()
        {
            return Err(corrupt("grant lifecycle timestamps do not match state"));
        }
        if let GrantBinding::DeviceEndpoint(_) = &record.binding {
            let original_secret = records.iter().any(|r| {
                r.approval_id == record.approval_id
                    && r.parent_network_grant_id.as_deref() == Some(record.grant_id.as_str())
            });
            if record
                .disclosure
                .as_ref()
                .is_none_or(|d| d.send_secrets != original_secret)
            {
                return Err(corrupt(
                    "endpoint original secret disclosure differs from its confirmation group",
                ));
            }
            if record.state != GrantState::Revoked
                && records.iter().any(|other| {
                    other.grant_id != record.grant_id
                        && other.state != GrantState::Revoked
                        && other.binding == record.binding
                })
            {
                return Err(corrupt("duplicate endpoint authority"));
            }
        }
        if let Some(binding) = record.binding.scoped()
            && binding.execution_mode
                != if record.parent_network_grant_id.is_some() {
                    "offline_api"
                } else {
                    "online_api"
                }
        {
            return Err(corrupt("scoped grant has incompatible execution authority"));
        }
        if let Some(parent_id) = &record.parent_network_grant_id {
            let secret = record
                .binding
                .scoped()
                .filter(|b| b.operation == GrantOperation::SendSecrets)
                .ok_or_else(|| corrupt("only a scoped secret grant may have an endpoint parent"))?;
            let parent = records
                .iter()
                .find(|r| &r.grant_id == parent_id)
                .ok_or_else(|| corrupt("secret endpoint parent is missing"))?;
            let GrantBinding::DeviceEndpoint(endpoint) = &parent.binding else {
                return Err(corrupt("secret parent is not an endpoint grant"));
            };
            let disclosure = record
                .disclosure
                .as_ref()
                .filter(|d| d.send_secrets)
                .ok_or_else(|| corrupt("scoped secret has no original secret-send disclosure"))?;
            endpoint::validate_disclosure(disclosure, endpoint)?;
            if secret.destination != endpoint.destination
                || secret.credential_binding != endpoint.credential_binding
                || secret.trust_binding != endpoint.trust_binding
                || (record.state == GrantState::Active && parent.state != GrantState::Active)
                || (record.state == GrantState::Pending && parent.state == GrantState::Revoked)
                || (parent.state == GrantState::Pending && parent.approval_id != record.approval_id)
            {
                return Err(corrupt(
                    "secret grant differs from its live endpoint parent",
                ));
            }
        } else if let Some(secret) = record
            .binding
            .scoped()
            .filter(|b| b.operation == GrantOperation::SendSecrets)
        {
            let mut expected = secret.clone();
            expected.operation = GrantOperation::Network;
            let network = records
                .iter()
                .find(|r| {
                    r.approval_id == record.approval_id
                        && r.binding == GrantBinding::Scoped(expected.clone())
                })
                .ok_or_else(|| corrupt("external secret lacks matching network approval"))?;
            if record.state != network.state
                || record.activated_at != network.activated_at
                || record.revoked_at != network.revoked_at
            {
                return Err(corrupt("external approval group states are inconsistent"));
            }
        }
        let same_operation = records
            .iter()
            .filter(|r| {
                r.approval_id == record.approval_id
                    && r.binding.operation() == record.binding.operation()
            })
            .count();
        if same_operation != 1 {
            return Err(corrupt("approval group has duplicate records"));
        }
    }
    Ok(())
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
    match &record.binding {
        GrantBinding::Scoped(binding) => {
            validate_binding(binding)?;
            if binding.operation == GrantOperation::SendSecrets
                && record.approval_id == record.grant_id
            {
                return Err(corrupt("grant record has an invalid approval identity"));
            }
            if record.disclosure.is_some() != record.parent_network_grant_id.is_some() {
                return Err(corrupt("scoped endpoint secret disclosure is inconsistent"));
            }
        }
        GrantBinding::DeviceEndpoint(binding) => {
            endpoint::validate_endpoint_record(binding, record)?
        }
    }
    validate_timestamp(&record.created_at)?;
    if let Some(value) = &record.activated_at {
        validate_timestamp(value)?;
    }
    if let Some(value) = &record.revoked_at {
        validate_timestamp(value)?;
    }
    Ok(())
}

fn validate_binding(binding: &ScopedGrantBinding) -> Result<()> {
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

    fn binding(root: &Path) -> ScopedGrantBinding {
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
        ScopedGrantBinding {
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
    fn device_endpoint_decode_is_closed_and_cannot_authorize_external_or_ambiguous_targets() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let path = root.join("nested/grants/grants.json");
        let control = kio_core::control::ControlHandle::default();
        let mut store = PrivateGrantStore::open_or_create(&path, &root, &control).unwrap();
        let binding = DeviceEndpointBinding {
            destination: "http://127.0.0.1:9443/embedding".into(),
            credential_binding: "none".into(),
            trust_binding: kio_adapter::local_peer::HTTP_UNAUTHENTICATED_TRUST_BINDING.into(),
        };
        let disclosure = EndpointDisclosure {
            tool_id: "qwen3_vl_embedding_local".into(),
            tool_profile_hash: "profile".into(),
            purpose: "embedding".into(),
            content_types: vec!["query text".into(), "document text".into(), "images".into()],
            applies_to: "all scopes on this device".into(),
            peer_authentication: "unauthenticated".into(),
            send_secrets: false,
        };
        store
            .begin_pending_endpoint(binding, disclosure, None, "2026-09-08T00:00:00Z".into())
            .unwrap();
        drop(store);
        let bytes = std::fs::read(&path).unwrap();
        let original: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        for destination in [
            "https://example.com:9443/embedding",
            "http://127.0.0.1/embedding",
            "http://user@127.0.0.1:9443/embedding",
            "http://127.0.0.1:9443/embedding?secret=x",
            "https://127.0.0.1:9443/embedding",
        ] {
            let mut changed = original.clone();
            changed["grants"][0]["binding"]["destination"] = serde_json::json!(destination);
            assert!(
                decode_store(&canonical_json_bytes(&changed).unwrap()).is_err(),
                "invalid endpoint identity admitted: {destination}"
            );
        }
        for (field, value) in [
            ("scope_id", "unrelated"),
            ("operation", "send_secrets"),
            ("trust_binding", "local-ca:wrong"),
        ] {
            let mut changed = original.clone();
            changed["grants"][0]["binding"][field] = serde_json::json!(value);
            assert!(
                decode_store(&canonical_json_bytes(&changed).unwrap()).is_err(),
                "ambiguous endpoint payload admitted: {field}"
            );
        }
        let mut duplicate = original.clone();
        let mut record = duplicate["grants"][0].clone();
        record["grant_id"] = serde_json::json!("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        record["approval_id"] = serde_json::json!("01ARZ3NDEKTSV4RRFFQ69G5FAW");
        duplicate["grants"].as_array_mut().unwrap().push(record);
        assert!(
            decode_store(&canonical_json_bytes(&duplicate).unwrap()).is_err(),
            "duplicate endpoint authority admitted"
        );
    }

    #[test]
    fn failed_endpoint_activation_cannot_publish_partial_authority_on_later_mutation() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let path = root.join("nested/grants/grants.json");
        let control = kio_core::control::ControlHandle::default();
        let mut store = PrivateGrantStore::open_or_create(&path, &root, &control).unwrap();
        let endpoint = DeviceEndpointBinding {
            destination: "http://127.0.0.1:9443/embedding".into(),
            credential_binding: "none".into(),
            trust_binding: kio_adapter::local_peer::HTTP_UNAUTHENTICATED_TRUST_BINDING.into(),
        };
        let disclosure = EndpointDisclosure {
            tool_id: "qwen3_vl_embedding_local".into(),
            tool_profile_hash: "profile".into(),
            purpose: "embedding".into(),
            content_types: vec!["query text".into(), "document text".into(), "images".into()],
            applies_to: "all scopes on this device".into(),
            peer_authentication: "unauthenticated".into(),
            send_secrets: true,
        };
        let mut secret = binding(&root);
        secret.operation = GrantOperation::SendSecrets;
        secret.execution_mode = "offline_api".into();
        secret.destination = endpoint.destination.clone();
        secret.credential_binding = endpoint.credential_binding.clone();
        secret.trust_binding = endpoint.trust_binding.clone();
        let (network, secret) = store
            .begin_pending_endpoint(
                endpoint,
                disclosure,
                Some(secret),
                "2026-09-08T00:00:00Z".into(),
            )
            .unwrap();
        let secret = secret.unwrap();
        let mut missing_secret = secret.clone();
        missing_secret.grant_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV".into();
        assert!(
            store
                .activate_endpoint_records(
                    &network,
                    Some(&missing_secret),
                    "2026-09-08T00:01:00Z".into()
                )
                .is_err()
        );
        let mut other = binding(&root);
        other.tool_id = "unrelated-tool".into();
        store
            .begin_pending_approval(other, false, "2026-09-08T00:02:00Z".into())
            .unwrap();
        drop(store);
        let reopened = PrivateGrantStore::open_readonly(&path, &root, &control)
            .unwrap()
            .unwrap();
        for id in [&network.grant_id, &secret.grant_id] {
            assert_eq!(
                reopened
                    .records()
                    .iter()
                    .find(|r| &r.grant_id == id)
                    .unwrap()
                    .state,
                GrantState::Pending,
                "failed multi-record confirmation leaked active authority into a later valid write"
            );
        }
    }

    #[test]
    fn writer_lifecycle_and_read_only_projection() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let path = root_path.join("nested/grants/grants.json");
        assert!(
            PrivateGrantStore::open_readonly(
                &path,
                &root_path,
                &kio_core::control::ControlHandle::default()
            )
            .unwrap()
            .is_none()
        );
        let mut store = PrivateGrantStore::open_or_create(
            &path,
            &root_path,
            &kio_core::control::ControlHandle::default(),
        )
        .unwrap();
        let (pending, _) = store
            .begin_pending_approval(binding(&root_path), false, "2026-09-08T00:00:00Z".into())
            .unwrap();
        store
            .activate_approval(&pending, None, "2026-09-08T00:01:00Z".into())
            .unwrap();
        drop(store);
        let active = PrivateGrantStore::open_readonly(
            &path,
            &root_path,
            &kio_core::control::ControlHandle::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(active.records().len(), 1);
        assert_eq!(
            active.records()[0].binding.scoped().unwrap().scope_id,
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
        let reopened = PrivateGrantStore::open_or_create(
            &path,
            &root_path,
            &kio_core::control::ControlHandle::default(),
        )
        .unwrap();
        assert_eq!(reopened.records().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn readonly_rejects_pending_atomic_workspace_until_writer_recovers_without_losing_grants() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let path = root_path.join("grants/grants.json");
        let mut store = PrivateGrantStore::open_or_create(
            &path,
            &root_path,
            &kio_core::control::ControlHandle::default(),
        )
        .unwrap();
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
        assert!(
            PrivateGrantStore::open_readonly(
                &path,
                &root_path,
                &kio_core::control::ControlHandle::default()
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), grants_before);
        assert_eq!(std::fs::read(&orphan).unwrap(), orphan_before);

        let recovered = PrivateGrantStore::open_or_create(
            &path,
            &root_path,
            &kio_core::control::ControlHandle::default(),
        )
        .unwrap();
        assert_eq!(recovered.records().len(), 1);
        drop(recovered);
        assert_eq!(std::fs::read(&path).unwrap(), grants_before);
        assert!(!orphan.exists());
        assert_eq!(
            directory.inspect_atomic().unwrap(),
            AtomicWorkspaceState::Clean
        );
        assert!(
            PrivateGrantStore::open_readonly(
                &path,
                &root_path,
                &kio_core::control::ControlHandle::default()
            )
            .unwrap()
            .is_some()
        );
    }

    #[test]
    fn revoke_is_terminal_and_idempotent() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let path = root_path.join("grants/grants.json");
        let mut store = PrivateGrantStore::open_or_create(
            &path,
            &root_path,
            &kio_core::control::ControlHandle::default(),
        )
        .unwrap();
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
        let mut store = PrivateGrantStore::open_or_create(
            &path,
            &root_path,
            &kio_core::control::ControlHandle::default(),
        )
        .unwrap();

        let original_binding = binding(&root_path);
        let original_identity = original_binding.directory_identity.clone();
        let (original, _) = store
            .begin_pending_approval(original_binding, false, "2026-09-08T00:00:00Z".into())
            .unwrap();
        store
            .activate_approval(&original, None, "2026-09-08T00:01:00Z".into())
            .unwrap();

        let mut replacement_binding = binding(&replacement_path);
        replacement_binding.scope_id = original.binding.scoped().unwrap().scope_id.clone();
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
                    &original.binding.scoped().unwrap().scope_id,
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
                    &original.binding.scoped().unwrap().scope_id,
                    &root_path,
                    &original_identity,
                    "2026-09-08T02:00:00Z".into(),
                )
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn scoped_secret_decode_rejects_approval_identity_equal_to_its_grant() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let path = root_path.join("grants/grants.json");
        let mut store = PrivateGrantStore::open_or_create(
            &path,
            &root_path,
            &kio_core::control::ControlHandle::default(),
        )
        .unwrap();
        let (network, secret) = store
            .begin_pending_approval(binding(&root_path), true, "2026-09-08T00:00:00Z".into())
            .unwrap();
        let secret = secret.unwrap();
        assert_ne!(network.approval_id, secret.grant_id);
        drop(store);
        let bytes = std::fs::read(&path).unwrap();
        assert!(decode_store(&bytes).is_ok());
        let mut changed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        for record in changed["grants"].as_array_mut().unwrap() {
            record["approval_id"] = serde_json::json!(secret.grant_id);
        }
        let error = decode_store(&canonical_json_bytes(&changed).unwrap())
            .expect_err("a paired network record cannot make an aliased secret approval valid");
        assert_eq!(error.error_code(), "KIO-E-GRANT-CORRUPT-001");
    }

    #[test]
    fn approval_group_publishes_and_activates_atomically() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let path = root_path.join("grants/grants.json");
        let mut store = PrivateGrantStore::open_or_create(
            &path,
            &root_path,
            &kio_core::control::ControlHandle::default(),
        )
        .unwrap();
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
        let mut store = PrivateGrantStore::open_or_create(
            &path,
            &root_path,
            &kio_core::control::ControlHandle::default(),
        )
        .unwrap();
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
        assert!(
            PrivateGrantStore::open_readonly(
                &directory.join(STORE_LEAF),
                &root_path,
                &kio_core::control::ControlHandle::default()
            )
            .is_err()
        );
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
        assert!(
            PrivateGrantStore::open_or_create(
                &path,
                &root_path,
                &kio_core::control::ControlHandle::default()
            )
            .is_err()
        );
        assert!(!path.exists());
    }

    #[test]
    fn retained_lock_blocks_second_writer_and_releases_on_drop() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let path = root_path.join("grants/grants.json");
        let first = PrivateGrantStore::open_or_create(
            &path,
            &root_path,
            &kio_core::control::ControlHandle::default(),
        )
        .unwrap();
        assert!(
            PrivateGrantStore::open_or_create(
                &path,
                &root_path,
                &kio_core::control::ControlHandle::default()
            )
            .is_err()
        );
        drop(first);
        assert!(
            PrivateGrantStore::open_or_create(
                &path,
                &root_path,
                &kio_core::control::ControlHandle::default()
            )
            .is_ok()
        );
    }
}

#[cfg(test)]
#[path = "grant_reserve_send_tests.rs"]
mod reserve_send_tests;
