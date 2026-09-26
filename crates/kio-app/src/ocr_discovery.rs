//! Provider-discovered Prepared units for OCR-from-scratch inputs.

use kio_adapter::types::{PreparedUnitHint, UnitKind};
use kio_core::{KioError, Result};
use kio_pipeline::prepare::{
    PreparedUnit, UnitType, canonical_unit_key, fingerprint_for_bytes, hash_bytes,
};

pub(crate) fn supports_ocr_from_scratch(media_type: &str) -> bool {
    matches!(
        media_type,
        "application/pdf" | "image/png" | "image/jpeg" | "image/webp" | "image/gif"
    )
}

pub(crate) fn prepared_units_from_ocr_discovery(
    hints: &[PreparedUnitHint],
    media_type: &str,
    raw_hash: &str,
    verified_raw_bytes: &[u8],
    preparation_profile_hash: &str,
) -> Result<Vec<PreparedUnit>> {
    if hints.is_empty() || hash_bytes(verified_raw_bytes) != raw_hash {
        return Err(KioError::schema(
            "OCR discovery must describe verified non-empty prepared units",
        ));
    }
    let expected_type = match media_type {
        "application/pdf" => UnitType::Page,
        "image/png" | "image/jpeg" | "image/webp" | "image/gif" => UnitType::Image,
        _ => return Err(KioError::schema("unsupported OCR discovery media type")),
    };
    if expected_type == UnitType::Image && hints.len() != 1 {
        return Err(KioError::schema(
            "standalone-image OCR must discover exactly one unit",
        ));
    }
    let fingerprint = fingerprint_for_bytes(&[], verified_raw_bytes);
    hints
        .iter()
        .enumerate()
        .map(|(index, hint)| {
            let expected_order = u64::try_from(index)
                .map_err(|_| KioError::schema("OCR discovery unit order exceeds u64"))?;
            let unit_type = match hint.unit_kind {
                UnitKind::Page => UnitType::Page,
                UnitKind::Image => UnitType::Image,
                _ => return Err(KioError::schema("unexpected OCR discovery unit type")),
            };
            let selector = if unit_type == UnitType::Page {
                (index + 1).to_string()
            } else {
                index.to_string()
            };
            if hint.order != expected_order
                || hint.prepared_hash != raw_hash
                || unit_type != expected_type
                || hint.unit_key != canonical_unit_key(unit_type, &selector)
            {
                return Err(KioError::schema(
                    "OCR discovery units are not canonical and contiguous",
                ));
            }
            Ok(PreparedUnit {
                order: hint.order,
                unit_key: hint.unit_key.clone(),
                unit_type,
                prepared_hash: hint.prepared_hash.clone(),
                preparation_profile_hash: preparation_profile_hash.to_owned(),
                fingerprint: fingerprint.clone(),
                mime: Some(media_type.to_owned()),
                page_number: (unit_type == UnitType::Page).then_some(hint.order + 1),
            })
        })
        .collect()
}
