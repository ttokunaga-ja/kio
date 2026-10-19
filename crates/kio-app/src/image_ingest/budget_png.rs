//! Share image 0.25.10 output storage with png 0.18.1's internal limit.
use super::{MAX_IMAGE_DECODED_BYTES, image_decode_limits, validate_decoded_dimensions};
use kio_core::{KioError, Result};

pub(super) fn png_limits(bytes: &[u8]) -> Result<image::Limits> {
    let err = || KioError::schema("PNG allocation preflight rejected invalid layout");
    if bytes.get(..8) != Some(b"\x89PNG\r\n\x1a\n")
        || bytes.get(8..16) != Some(&[0, 0, 0, 13, b'I', b'H', b'D', b'R'])
    {
        return Err(err());
    }
    let header = bytes.get(16..29).ok_or_else(err)?;
    let width = u32::from_be_bytes(header[..4].try_into().unwrap());
    let height = u32::from_be_bytes(header[4..8].try_into().unwrap());
    validate_decoded_dimensions(width, height)?;
    let depth = header[8];
    let color = header[9];
    let mut trns = false;
    let mut pos = 33;
    loop {
        let h = bytes.get(pos..pos + 8).ok_or_else(err)?;
        let length = u32::from_be_bytes(h[..4].try_into().unwrap()) as usize;
        let end = pos
            .checked_add(12)
            .and_then(|p| p.checked_add(length))
            .ok_or_else(err)?;
        if end > bytes.len() {
            return Err(err());
        }
        match &h[4..8] {
            b"tRNS" => trns = true,
            b"IDAT" => break,
            b"IEND" => return Err(err()),
            _ => (),
        }
        pos = end;
    }
    let channels = match color {
        0 => {
            if trns {
                2
            } else {
                1
            }
        }
        2 | 3 => {
            if trns {
                4
            } else {
                3
            }
        }
        4 => 2,
        6 => 4,
        _ => return Err(err()),
    };
    let bytes_per_channel = if depth == 16 { 2 } else { 1 };
    let output = u64::from(width) * u64::from(height) * channels * bytes_per_channel;
    let remaining = MAX_IMAGE_DECODED_BYTES
        .checked_sub(output)
        .ok_or_else(err)?;
    let mut limits = image_decode_limits();
    limits.max_alloc = Some(remaining);
    Ok(limits)
}
