//! Provider metering is usable only when its complete modality report agrees.
use crate::types::{AdapterUsage, BillableUnit, BillableUnitKind, EmbeddingUsage};
use serde_json::Value;

fn count(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        let text = value.as_str()?;
        (!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| text.parse().ok())
            .flatten()
    })
}

pub(crate) fn parse(
    response: &Value,
    max_tokens: u64,
    expected_kind: Option<BillableUnitKind>,
) -> EmbeddingUsage {
    metered(response, max_tokens, expected_kind).unwrap_or(EmbeddingUsage::Estimated)
}

fn metered(
    response: &Value,
    max_tokens: u64,
    expected_kind: Option<BillableUnitKind>,
) -> Option<EmbeddingUsage> {
    let usage = response.get("usageMetadata")?;
    let total = count(usage.get("promptTokenCount")?)?;
    if total > max_tokens {
        return None;
    }
    let details = usage.get("promptTokenDetails")?.as_array()?;
    if details.is_empty() || details.len() > 2 {
        return None;
    }
    let mut text = None;
    let mut image = None;
    let mut sum = 0_u64;
    for detail in details {
        let (kind, slot) = match detail.get("modality")?.as_str()? {
            "TEXT" => (BillableUnitKind::TokensIn, &mut text),
            "IMAGE" => (BillableUnitKind::ImageTokensIn, &mut image),
            _ => return None,
        };
        if expected_kind.is_some_and(|expected| expected != kind) || slot.is_some() {
            return None;
        }
        let value = count(detail.get("tokenCount")?)?;
        sum = sum.checked_add(value)?;
        *slot = Some(value);
    }
    if sum != total {
        return None;
    }
    let mut billable_units = Vec::with_capacity(details.len());
    if let Some(count) = text {
        billable_units.push(BillableUnit {
            kind: BillableUnitKind::TokensIn,
            count,
        });
    }
    if let Some(count) = image {
        billable_units.push(BillableUnit {
            kind: BillableUnitKind::ImageTokensIn,
            count,
        });
    }
    Some(EmbeddingUsage::Metered {
        usage: AdapterUsage::BillableUnits { billable_units },
    })
}
