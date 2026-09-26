//! Durable, fail-closed provider-acceptance budget reservations.
//!
//! These are executor-format records exported only from a verified append-only
//! Git authority commit. A campaign binding must already be present before a
//! paid request can reserve money; Actions artifacts are diagnostic receipts,
//! never budget authority.

use std::{
    fs::{self, OpenOptions},
    io::Read,
    path::Path,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const CAMPAIGN_SCHEMA: &str = "kio.provider-budget-campaign/v1";
pub const RESERVATION_SCHEMA: &str = "kio.provider-budget-reservation/v1";
const MAX_RECORD_BYTES: u64 = 16 * 1024;
/// The currently approved campaign ceiling is USD 10 total per provider.
pub const MAX_CAMPAIGN_MICROUSD: u64 = 10_000_000;
/// A provider request group may reserve at most USD 0.10 until a separately
/// bounded executor proves a different worst-case cost.
pub const MAX_REQUEST_GROUP_MICROUSD: u64 = 100_000;

#[derive(Debug, Error)]
pub enum ProviderBudgetError {
    #[error("provider budget input is invalid: {0}")]
    Invalid(String),
    #[error("provider budget I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("provider budget JSON: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CampaignBinding {
    pub schema: String,
    pub campaign_id: String,
    pub provider: String,
    pub cap_microusd: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reservation {
    pub schema: String,
    pub campaign_id: String,
    pub candidate_sha: String,
    pub provider: String,
    pub allocation_id: String,
    pub allocated_microusd: u64,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReservationSummary {
    pub status: String,
    pub provider: String,
    pub allocated_microusd: u64,
    pub committed_microusd: u64,
    pub remaining_microusd: u64,
}

pub fn initialize_campaign(
    state_dir: &Path,
    campaign_id: &str,
    provider: &str,
    cap_microusd: u64,
) -> Result<CampaignBinding, ProviderBudgetError> {
    validate_campaign(campaign_id, provider)?;
    validate_campaign_cap(cap_microusd)?;
    require_directory(state_dir)?;
    let binding = CampaignBinding {
        schema: CAMPAIGN_SCHEMA.to_owned(),
        campaign_id: campaign_id.to_owned(),
        provider: provider.to_owned(),
        cap_microusd,
    };
    write_create_only(&state_dir.join("campaign.json"), &binding)?;
    Ok(binding)
}

pub fn reserve(
    state_dir: &Path,
    campaign_id: &str,
    candidate_sha: &str,
    provider: &str,
    allocation_id: &str,
    cap_microusd: u64,
    amount_microusd: u64,
) -> Result<ReservationSummary, ProviderBudgetError> {
    validate_campaign(campaign_id, provider)?;
    validate_candidate_sha(candidate_sha)?;
    validate_identifier(allocation_id, "allocation id")?;
    validate_campaign_cap(cap_microusd)?;
    validate_request_group_amount(amount_microusd)?;
    require_directory(state_dir)?;
    let binding = read_binding(state_dir)?;
    if binding.campaign_id != campaign_id
        || binding.provider != provider
        || binding.cap_microusd != cap_microusd
    {
        return Err(invalid(
            "prior campaign artifact does not match this immutable provider campaign cap",
        ));
    }
    let reservations = read_reservations(state_dir)?;
    let mut committed = 0_u64;
    for prior in &reservations {
        validate_reservation(prior)?;
        if prior.campaign_id != campaign_id || prior.provider != provider {
            return Err(invalid(
                "reservation state belongs to a different provider campaign",
            ));
        }
        if prior.allocation_id == allocation_id {
            return Err(invalid(
                "allocation is already reserved; unknown work must not be retried",
            ));
        }
        committed = committed
            .checked_add(prior.allocated_microusd)
            .ok_or_else(|| invalid("reservation total overflow"))?;
    }
    let next = committed
        .checked_add(amount_microusd)
        .ok_or_else(|| invalid("reservation total overflow"))?;
    if next > cap_microusd {
        return Err(invalid("campaign provider budget would be exceeded"));
    }
    let reservation = Reservation {
        schema: RESERVATION_SCHEMA.to_owned(),
        campaign_id: campaign_id.to_owned(),
        candidate_sha: candidate_sha.to_owned(),
        provider: provider.to_owned(),
        allocation_id: allocation_id.to_owned(),
        allocated_microusd: amount_microusd,
        state: "reserved_unknown".to_owned(),
    };
    write_create_only(
        &state_dir.join(format!("reservation-{allocation_id}.json")),
        &reservation,
    )?;
    Ok(ReservationSummary {
        status: reservation.state,
        provider: provider.to_owned(),
        allocated_microusd: amount_microusd,
        committed_microusd: next,
        remaining_microusd: cap_microusd - next,
    })
}

/// Validate the complete reconstructed campaign state before a provider sends
/// a request for one previously uploaded, unknown reservation.
///
/// This reads every reservation, so a malformed or over-cap prior allocation
/// cannot be hidden behind an otherwise valid target record.
pub fn validate_exact_reservation(
    state_dir: &Path,
    campaign_id: &str,
    candidate_sha: &str,
    provider: &str,
    allocation_id: &str,
) -> Result<Reservation, ProviderBudgetError> {
    validate_campaign(campaign_id, provider)?;
    validate_candidate_sha(candidate_sha)?;
    validate_identifier(allocation_id, "allocation id")?;
    require_directory(state_dir)?;

    let binding = read_binding(state_dir)?;
    if binding.campaign_id != campaign_id || binding.provider != provider {
        return Err(invalid(
            "prior campaign artifact does not match the requested provider campaign",
        ));
    }

    let mut committed = 0_u64;
    let mut matched = None;
    for reservation in read_reservations(state_dir)? {
        validate_reservation(&reservation)?;
        if reservation.campaign_id != campaign_id || reservation.provider != provider {
            return Err(invalid(
                "reservation state belongs to a different provider campaign",
            ));
        }
        committed = committed
            .checked_add(reservation.allocated_microusd)
            .ok_or_else(|| invalid("reservation total overflow"))?;
        if reservation.allocation_id == allocation_id {
            if reservation.candidate_sha != candidate_sha {
                return Err(invalid(
                    "requested allocation belongs to a different candidate",
                ));
            }
            if matched.replace(reservation.clone()).is_some() {
                return Err(invalid("requested allocation appears more than once"));
            }
        }
    }
    if committed > binding.cap_microusd {
        return Err(invalid(
            "reconstructed campaign provider budget exceeds its cap",
        ));
    }
    matched.ok_or_else(|| invalid("requested durable provider reservation is missing"))
}

fn read_binding(state_dir: &Path) -> Result<CampaignBinding, ProviderBudgetError> {
    let path = state_dir.join("campaign.json");
    let binding: CampaignBinding = read_json_file(&path).map_err(|error| match error {
        ProviderBudgetError::Io(source) if source.kind() == std::io::ErrorKind::NotFound => {
            invalid("prior campaign artifact is missing; refusing to reset budget")
        }
        other => other,
    })?;
    if binding.schema != CAMPAIGN_SCHEMA {
        return Err(invalid("prior campaign artifact has an invalid schema"));
    }
    validate_campaign(&binding.campaign_id, &binding.provider)?;
    validate_campaign_cap(binding.cap_microusd)?;
    Ok(binding)
}

fn read_reservations(state_dir: &Path) -> Result<Vec<Reservation>, ProviderBudgetError> {
    let mut paths = fs::read_dir(state_dir)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort();
    paths
        .into_iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("reservation-") && name.ends_with(".json"))
        })
        .map(|path| {
            let reservation: Reservation = read_json_file(&path)?;
            let expected_name = format!("reservation-{}.json", reservation.allocation_id);
            if path.file_name().and_then(|name| name.to_str()) != Some(expected_name.as_str()) {
                return Err(invalid(
                    "reservation filename does not bind its allocation id",
                ));
            }
            Ok(reservation)
        })
        .collect()
}

fn read_json_file<T: for<'a> Deserialize<'a>>(path: &Path) -> Result<T, ProviderBudgetError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_RECORD_BYTES {
        return Err(invalid("campaign artifact is not a bounded regular file"));
    }
    let mut file = fs::File::open(path)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take(MAX_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(invalid("campaign artifact exceeds the bounded size"));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

fn write_create_only<T: Serialize>(path: &Path, value: &T) -> Result<(), ProviderBudgetError> {
    let bytes = serde_json::to_vec(value)?;
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    use std::io::Write;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn require_directory(path: &Path) -> Result<(), ProviderBudgetError> {
    if !path.is_dir() {
        return Err(invalid("state directory must already exist"));
    }
    Ok(())
}

fn validate_reservation(reservation: &Reservation) -> Result<(), ProviderBudgetError> {
    if reservation.schema != RESERVATION_SCHEMA || reservation.state != "reserved_unknown" {
        return Err(invalid("reservation schema or state is invalid"));
    }
    validate_campaign(&reservation.campaign_id, &reservation.provider)?;
    validate_candidate_sha(&reservation.candidate_sha)?;
    validate_identifier(&reservation.allocation_id, "allocation id")?;
    validate_request_group_amount(reservation.allocated_microusd)
}

fn validate_campaign(campaign_id: &str, provider: &str) -> Result<(), ProviderBudgetError> {
    validate_identifier(campaign_id, "campaign id")?;
    if !matches!(provider, "mistral" | "gemini") {
        return Err(invalid("provider must be mistral or gemini"));
    }
    Ok(())
}

fn validate_candidate_sha(candidate_sha: &str) -> Result<(), ProviderBudgetError> {
    if candidate_sha.len() != 40
        || !candidate_sha
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid(
            "candidate SHA must be exactly 40 lowercase hexadecimal characters",
        ));
    }
    Ok(())
}

fn validate_identifier(value: &str, label: &str) -> Result<(), ProviderBudgetError> {
    if value.is_empty()
        || value.len() > 96
        || !value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'_' | b'-')
                    && (index != 0 || byte.is_ascii_alphanumeric())
        })
    {
        return Err(invalid(format!(
            "{label} must be a bounded ASCII identifier"
        )));
    }
    Ok(())
}

fn validate_campaign_cap(value: u64) -> Result<(), ProviderBudgetError> {
    if value == 0 || value > MAX_CAMPAIGN_MICROUSD {
        return Err(invalid(
            "campaign cap must be a positive value no greater than USD 10",
        ));
    }
    Ok(())
}

fn validate_request_group_amount(value: u64) -> Result<(), ProviderBudgetError> {
    if value == 0 || value > MAX_REQUEST_GROUP_MICROUSD {
        return Err(invalid(
            "request-group allocation must be a positive value no greater than USD 0.10",
        ));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> ProviderBudgetError {
    ProviderBudgetError::Invalid(message.into())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{
        ProviderBudgetError, RESERVATION_SCHEMA, initialize_campaign, reserve,
        validate_exact_reservation,
    };

    const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn reservations_are_campaign_global_across_candidate_fixes() {
        let dir = tempfile::tempdir().unwrap();
        initialize_campaign(dir.path(), "campaign-1", "mistral", 10_000_000).unwrap();
        let first = reserve(
            dir.path(),
            "campaign-1",
            SHA_A,
            "mistral",
            "mistral-linux-a",
            10_000_000,
            100_000,
        )
        .unwrap();
        let second = reserve(
            dir.path(),
            "campaign-1",
            SHA_B,
            "mistral",
            "mistral-linux-b",
            10_000_000,
            100_000,
        )
        .unwrap();
        assert_eq!(first.committed_microusd, 100_000);
        assert_eq!(second.committed_microusd, 200_000);
        assert!(matches!(
            reserve(
                dir.path(),
                "campaign-1",
                SHA_B,
                "mistral",
                "mistral-expensive",
                10_000_000,
                100_001,
            ),
            Err(ProviderBudgetError::Invalid(_))
        ));
        assert!(matches!(
            reserve(
                dir.path(),
                "campaign-1",
                SHA_B,
                "mistral",
                "mistral-linux-b",
                10_000_000,
                100_000,
            ),
            Err(ProviderBudgetError::Invalid(_))
        ));
    }

    #[test]
    fn caller_cannot_change_a_campaigns_immutable_cap() {
        let dir = tempfile::tempdir().unwrap();
        initialize_campaign(dir.path(), "campaign-1", "gemini", 10_000_000).unwrap();
        assert!(matches!(
            reserve(
                dir.path(),
                "campaign-1",
                SHA_A,
                "gemini",
                "gemini-linux-a",
                9_999_999,
                100_000,
            ),
            Err(ProviderBudgetError::Invalid(_))
        ));
    }

    #[test]
    fn exact_reservation_scans_the_campaign_and_permits_a_lower_immutable_cap() {
        let dir = tempfile::tempdir().unwrap();
        initialize_campaign(dir.path(), "campaign-1", "mistral", 100_000).unwrap();
        reserve(
            dir.path(),
            "campaign-1",
            SHA_A,
            "mistral",
            "mistral-linux-a",
            100_000,
            100_000,
        )
        .unwrap();
        let exact = validate_exact_reservation(
            dir.path(),
            "campaign-1",
            SHA_A,
            "mistral",
            "mistral-linux-a",
        )
        .unwrap();
        assert_eq!(exact.allocated_microusd, 100_000);

        fs::write(
            dir.path().join("reservation-mistral-hidden.json"),
            format!(
                "{{\"schema\":\"{RESERVATION_SCHEMA}\",\"campaign_id\":\"campaign-1\",\"candidate_sha\":\"{SHA_B}\",\"provider\":\"mistral\",\"allocation_id\":\"mistral-hidden\",\"allocated_microusd\":1,\"state\":\"reserved_unknown\"}}\n"
            ),
        )
        .unwrap();
        assert!(matches!(
            validate_exact_reservation(
                dir.path(),
                "campaign-1",
                SHA_A,
                "mistral",
                "mistral-linux-a",
            ),
            Err(ProviderBudgetError::Invalid(_))
        ));
    }

    #[test]
    fn reservation_requires_a_prior_campaign_artifact() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            reserve(
                dir.path(),
                "campaign-1",
                SHA_A,
                "mistral",
                "mistral-linux",
                10_000_000,
                100_000,
            ),
            Err(ProviderBudgetError::Invalid(_))
        ));
    }
}
