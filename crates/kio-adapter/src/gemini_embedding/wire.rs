//! Gemini's bounded wire representation; decoded-source validation is shared
//! by image ingestion before the adapter receives the verified original bytes.
use crate::types::{
    BillableUnitKind, EmbeddingContent, EmbeddingItem, MAX_EMBEDDING_ITEM_BYTES,
    MAX_EMBEDDING_REQUEST_BYTES,
};
use crate::{AdapterError, Result};
use base64::Engine as _;
use serde_json::{Value, json};

pub(crate) const MAX_ENCODED_BYTES: usize = 16 * 1024 * 1024;

pub(crate) fn validate_contents<'a>(
    contents: impl Iterator<Item = &'a EmbeddingContent>,
) -> Result<(BillableUnitKind, usize)> {
    let mut count = 0_usize;
    let mut total = 0_usize;
    let mut kind = None;
    for content in contents {
        let (current, bytes) = match content {
            EmbeddingContent::Text { text } => (BillableUnitKind::TokensIn, text.len()),
            EmbeddingContent::Image { bytes, mime } => {
                validate_image(bytes, mime)?;
                (BillableUnitKind::ImageTokensIn, bytes.len())
            }
        };
        if kind.is_some_and(|previous| previous != current) {
            return Err(invalid("Gemini text and image require separate jobs"));
        }
        kind = Some(current);
        count = count
            .checked_add(1)
            .ok_or_else(|| invalid("member count overflow"))?;
        total = total
            .checked_add(bytes)
            .ok_or_else(|| invalid("input bytes overflow"))?;
        if bytes > MAX_EMBEDDING_ITEM_BYTES || total > MAX_EMBEDDING_REQUEST_BYTES {
            return Err(invalid("Gemini input exceeds the source byte limit"));
        }
        if current == BillableUnitKind::ImageTokensIn && count != 1 {
            return Err(invalid("Gemini permits one image member per HTTP call"));
        }
    }
    Ok((
        kind.ok_or_else(|| invalid("Gemini requires at least one member"))?,
        count,
    ))
}

fn validate_image(bytes: &[u8], mime: &str) -> Result<()> {
    let recognized = match mime {
        "image/png" => {
            bytes.len() >= 33
                && bytes.starts_with(b"\x89PNG\r\n\x1a\n")
                && bytes[8..16] == [0, 0, 0, 13, b'I', b'H', b'D', b'R']
        }
        "image/jpeg" => {
            bytes.len() >= 4 && bytes.starts_with(&[0xff, 0xd8]) && bytes.ends_with(&[0xff, 0xd9])
        }
        _ => false,
    };
    if !recognized {
        return Err(invalid("Gemini image bytes disagree with PNG/JPEG MIME"));
    }
    let encoded_len = bytes
        .len()
        .checked_add(2)
        .and_then(|length| (length / 3).checked_mul(4))
        .ok_or_else(|| invalid("Gemini image encoded length overflow"))?;
    if encoded_len > MAX_ENCODED_BYTES {
        return Err(invalid("Gemini image exceeds the encoded body byte limit"));
    }
    Ok(())
}

pub(crate) fn content(content: &EmbeddingContent) -> Value {
    let part = match content {
        EmbeddingContent::Text { text } => json!({"text": text}),
        EmbeddingContent::Image { bytes, mime } => json!({"inlineData": {
            "mimeType": mime,
            "data": base64::engine::general_purpose::STANDARD.encode(bytes)
        }}),
    };
    json!({"parts": [part]})
}

pub(crate) fn check_body(body: &Value) -> Result<()> {
    let encoded =
        serde_json::to_vec(body).map_err(|_| invalid("Gemini body serialization failed"))?;
    if encoded.len() > MAX_ENCODED_BYTES {
        return Err(invalid("Gemini encoded body exceeds the 16 MiB byte limit"));
    }
    Ok(())
}

pub(crate) fn sync_body(
    model: &str,
    dimensions: u32,
    items: &[EmbeddingItem],
) -> Result<(Value, BillableUnitKind, usize)> {
    let (kind, count) = validate_contents(items.iter().map(|item| &item.content))?;
    let requests = items
        .iter()
        .map(|item| {
            json!({
                "model": format!("models/{model}"),
                "content": content(&item.content),
                "embedContentConfig": {"outputDimensionality": dimensions}
            })
        })
        .collect::<Vec<_>>();
    let body = json!({"requests": requests});
    check_body(&body)?;
    Ok((body, kind, count))
}

fn invalid(message: &str) -> AdapterError {
    AdapterError::ContractViolation(message.to_owned())
}
