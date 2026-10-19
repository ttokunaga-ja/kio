//! Inspect frame declarations before gif 0.14.2 allocates frame rasters.
use super::{
    MAX_GIF_FRAMES, MAX_IMAGE_DECODED_BYTES, MAX_IMAGE_PIXELS, checked_image_pixels,
    validate_decoded_dimensions,
};
use kio_core::{KioError, Result};

pub(super) fn validate_gif_layout(bytes: &[u8]) -> Result<()> {
    let err = || KioError::schema("GIF allocation preflight rejected invalid layout");
    let h = bytes.get(..13).ok_or_else(err)?;
    if &h[..6] != b"GIF87a" && &h[..6] != b"GIF89a" {
        return Err(err());
    }
    let width = u32::from(u16::from_le_bytes([h[6], h[7]]));
    let height = u32::from(u16::from_le_bytes([h[8], h[9]]));
    validate_decoded_dimensions(width, height)?;
    let canvas = checked_image_pixels(width, height)?;
    let mut pos = 13 + palette_len(h[10]);
    let mut frames = 0;
    let mut indexed_capacity = 0_u64;
    loop {
        let marker = *bytes.get(pos).ok_or_else(err)?;
        pos += 1;
        match marker {
            59 => {
                if frames == 0 {
                    return Err(err());
                }
                return Ok(());
            }
            33 => {
                pos += 1; // extension label; sub-block sequence follows
                subblocks(bytes, &mut pos)?;
            }
            44 => {
                let f = bytes.get(pos..pos + 9).ok_or_else(err)?;
                let fw = u32::from(u16::from_le_bytes([f[4], f[5]]));
                let fh = u32::from(u16::from_le_bytes([f[6], f[7]]));
                let frame = checked_image_pixels(fw, fh)?;
                frames += 1;
                if frames > MAX_GIF_FRAMES || frames * canvas > MAX_IMAGE_PIXELS {
                    return Err(KioError::schema(
                        "GIF exceeds the supported frame or total decoded pixel limit",
                    ));
                }
                // image's iterator retains the disposal canvas, the decoded
                // subframe, and (only for offset/subsize frames) a composition.
                let full = f[..4] == [0, 0, 0, 0] && fw == width && fh == height;
                // gif's RGBA converter also retains an indexed intermediate:
                // one row for interlaced frames, the whole frame otherwise.
                // Vec::resize grows to max(required, 2*old_capacity, 8).
                let needed = if f[8] & 64 != 0 { u64::from(fw) } else { frame };
                let old_capacity = indexed_capacity;
                if needed > indexed_capacity {
                    indexed_capacity = needed.max(2 * indexed_capacity).max(8);
                }
                // Locked weezl: 4096*(Link{u16,u8,u8}+u16 depth+u8
                // reconstruction), two states during code-size replacement;
                // 80KiB bounds these, boxed state, palettes and BufReader.
                const FIXED_SCRATCH: u64 = 80 * 1024;
                let decoding_peak = 4 * (canvas + frame)
                    + indexed_capacity
                    + if indexed_capacity != old_capacity {
                        old_capacity
                    } else {
                        0
                    };
                let compositing_peak =
                    4 * (canvas + frame + if full { 0 } else { canvas }) + indexed_capacity;
                if decoding_peak.max(compositing_peak) + FIXED_SCRATCH > MAX_IMAGE_DECODED_BYTES {
                    return Err(KioError::schema(
                        "GIF decoded allocation exceeds the supported limit",
                    ));
                }
                pos += 9 + palette_len(f[8]);
                pos += 1; // LZW minimum code size
                subblocks(bytes, &mut pos)?;
            }
            _ => return Err(err()),
        }
    }
}
fn palette_len(packed: u8) -> usize {
    if packed & 128 == 0 {
        0
    } else {
        3 * (1 << ((packed & 7) + 1))
    }
}
fn subblocks(bytes: &[u8], pos: &mut usize) -> Result<()> {
    loop {
        let len = usize::from(
            *bytes
                .get(*pos)
                .ok_or_else(|| KioError::schema("GIF sub-block is truncated"))?,
        );
        *pos += 1;
        if len == 0 {
            return Ok(());
        }
        *pos = pos
            .checked_add(len)
            .filter(|p| *p <= bytes.len())
            .ok_or_else(|| KioError::schema("GIF sub-block is truncated"))?;
    }
}
