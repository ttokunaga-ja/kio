use super::*;

pub(super) fn validate_endpoint_record(
    binding: &DeviceEndpointBinding,
    record: &GrantRecord,
) -> Result<()> {
    let destination = kio_adapter::local_peer::validate_local_url(&binding.destination)
        .map_err(crate::object_persistence::adapter_to_kio)?;
    if destination != binding.destination || binding.credential_binding != "none" {
        return Err(corrupt(
            "endpoint destination or consumed credential identity is invalid",
        ));
    }
    kio_adapter::local_peer::validate_local_trust_binding(&destination, &binding.trust_binding)
        .map_err(crate::object_persistence::adapter_to_kio)?;
    if destination.starts_with("https://") {
        let fields = binding.trust_binding.split(':').collect::<Vec<_>>();
        if fields.len() != 6
            || fields[0] != "local-ca"
            || fields[1] != "v1"
            || !fields[2]
                .strip_prefix('g')
                .is_some_and(|v| v.parse::<u64>().is_ok_and(|n| n > 0 && n.to_string() == v))
            || !is_ulid(fields[3])
            || fields[4] != "sha256"
            || fields[5].len() != 64
            || !fields[5]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(corrupt(
                "endpoint HTTPS trust is not a managed lifecycle identity",
            ));
        }
    }
    let disclosure = record
        .disclosure
        .as_ref()
        .ok_or_else(|| corrupt("endpoint grant has no original disclosure"))?;
    validate_disclosure(disclosure, binding)?;
    if record.parent_network_grant_id.is_some() {
        return Err(corrupt("endpoint normal grant cannot have a parent"));
    }
    Ok(())
}

pub(super) fn validate_disclosure(
    disclosure: &EndpointDisclosure,
    binding: &DeviceEndpointBinding,
) -> Result<()> {
    let expected: &[&str] = match disclosure.purpose.as_str() {
        "embedding" if disclosure.tool_id == "qwen3_vl_embedding_local" => {
            &["query text", "document text", "images"]
        }
        "markdown" if disclosure.tool_id == "paddleocr_vl_local" => {
            &["document page images", "document text"]
        }
        _ => {
            return Err(corrupt(
                "endpoint disclosure purpose does not match a local adapter",
            ));
        }
    };
    if disclosure.tool_profile_hash.is_empty()
        || disclosure
            .content_types
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .as_slice()
            != expected
        || disclosure.applies_to != "all scopes on this device"
        || disclosure.peer_authentication
            != if binding.destination.starts_with("http://") {
                "unauthenticated"
            } else {
                "managed_ca"
            }
    {
        return Err(corrupt("endpoint original disclosure is invalid"));
    }
    Ok(())
}

impl PrivateGrantStore {
    pub fn begin_pending_endpoint(
        &mut self,
        binding: DeviceEndpointBinding,
        disclosure: EndpointDisclosure,
        secret: Option<ScopedGrantBinding>,
        now: String,
    ) -> Result<(GrantRecord, Option<GrantRecord>)> {
        if self.records.iter().any(|r| {
            r.state != GrantState::Revoked
                && r.binding == GrantBinding::DeviceEndpoint(binding.clone())
        }) {
            return Err(invalid(
                "endpoint authority already exists; resume its exact pending intent",
            ));
        }
        let network = GrantRecord {
            schema_version: SCHEMA_VERSION,
            grant_id: new_ulid(&self.path),
            approval_id: new_ulid(&self.path),
            device_instance_id: self.device_instance_id.clone(),
            state: GrantState::Pending,
            binding: GrantBinding::DeviceEndpoint(binding),
            disclosure: Some(disclosure.clone()),
            parent_network_grant_id: None,
            created_at: now.clone(),
            activated_at: None,
            revoked_at: None,
        };
        let secret = secret.map(|binding| GrantRecord {
            schema_version: SCHEMA_VERSION,
            grant_id: new_ulid(&self.path),
            approval_id: network.approval_id.clone(),
            device_instance_id: self.device_instance_id.clone(),
            state: GrantState::Pending,
            binding: GrantBinding::Scoped(binding),
            disclosure: Some(disclosure.clone()),
            parent_network_grant_id: Some(network.grant_id.clone()),
            created_at: now,
            activated_at: None,
            revoked_at: None,
        });
        self.add_endpoint_records(&network, secret.as_ref())?;
        Ok((network, secret))
    }

    pub fn begin_pending_endpoint_secret(
        &mut self,
        parent: &GrantRecord,
        binding: ScopedGrantBinding,
        disclosure: EndpointDisclosure,
        now: String,
    ) -> Result<GrantRecord> {
        if parent.state != GrantState::Active || !self.records.contains(parent) {
            return Err(invalid(
                "secret approval requires the exact active endpoint grant",
            ));
        }
        let record = GrantRecord {
            schema_version: SCHEMA_VERSION,
            grant_id: new_ulid(&self.path),
            approval_id: new_ulid(&self.path),
            device_instance_id: self.device_instance_id.clone(),
            state: GrantState::Pending,
            binding: GrantBinding::Scoped(binding),
            disclosure: Some(disclosure),
            parent_network_grant_id: Some(parent.grant_id.clone()),
            created_at: now,
            activated_at: None,
            revoked_at: None,
        };
        self.add_endpoint_records(&record, None)?;
        Ok(record)
    }

    fn add_endpoint_records(
        &mut self,
        first: &GrantRecord,
        second: Option<&GrantRecord>,
    ) -> Result<()> {
        let before = self.records.clone();
        self.records.push(first.clone());
        if let Some(second) = second {
            self.records.push(second.clone());
        }
        if let Err(error) = validate_records(&self.device_instance_id, &self.records)
            .and_then(|_| self.persist(Publication::Upsert))
        {
            self.records = before;
            return Err(error);
        }
        Ok(())
    }

    pub fn activate_endpoint_records(
        &mut self,
        pending: &GrantRecord,
        secret: Option<&GrantRecord>,
        now: String,
    ) -> Result<()> {
        validate_timestamp(&now)?;
        match &pending.binding {
            GrantBinding::DeviceEndpoint(_) => {
                if secret.is_some_and(|record| {
                    record.parent_network_grant_id.as_deref() != Some(pending.grant_id.as_str())
                        || record.binding.operation() != GrantOperation::SendSecrets
                }) {
                    return Err(invalid("secret confirmation has another endpoint parent"));
                }
            }
            GrantBinding::Scoped(binding)
                if binding.operation == GrantOperation::SendSecrets
                    && pending.parent_network_grant_id.is_some()
                    && secret.is_none() => {}
            _ => {
                return Err(invalid(
                    "confirmation is not endpoint or scoped endpoint-secret authority",
                ));
            }
        }
        // Validate the complete exact pending set before changing any record.
        // A failed second member must not leak activation into a later write.
        let indexes = std::iter::once(pending)
            .chain(secret)
            .map(|expected| {
                if expected.state != GrantState::Pending {
                    return Err(invalid("only pending confirmation can activate"));
                }
                self.records
                    .iter()
                    .position(|record| record == expected)
                    .ok_or_else(|| invalid("pending endpoint confirmation changed"))
            })
            .collect::<Result<Vec<_>>>()?;
        if indexes
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != indexes.len()
        {
            return Err(invalid("confirmation contains the same record twice"));
        }
        validate_records(&self.device_instance_id, &self.records)?;
        let before = self.records.clone();
        for index in indexes {
            self.records[index].state = GrantState::Active;
            self.records[index].activated_at = Some(now.clone());
        }
        if let Err(error) = validate_records(&self.device_instance_id, &self.records)
            .and_then(|_| self.persist(Publication::Replace))
        {
            self.records = before;
            return Err(error);
        }
        Ok(())
    }

    pub fn revoke_endpoint(
        &mut self,
        binding: &DeviceEndpointBinding,
        now: String,
    ) -> Result<Vec<String>> {
        validate_timestamp(&now)?;
        let parents = self
            .records
            .iter()
            .filter(|r| r.binding == GrantBinding::DeviceEndpoint(binding.clone()))
            .map(|r| r.grant_id.clone())
            .collect::<Vec<_>>();
        let before = self.records.clone();
        let mut changed = Vec::new();
        for record in &mut self.records {
            if record.state != GrantState::Revoked
                && (parents.contains(&record.grant_id)
                    || record
                        .parent_network_grant_id
                        .as_ref()
                        .is_some_and(|id| parents.contains(id)))
            {
                record.state = GrantState::Revoked;
                record.revoked_at = Some(now.clone());
                changed.push(record.grant_id.clone());
            }
        }
        if !changed.is_empty()
            && let Err(error) = self.persist(Publication::Replace)
        {
            self.records = before;
            return Err(error);
        }
        Ok(changed)
    }
}
