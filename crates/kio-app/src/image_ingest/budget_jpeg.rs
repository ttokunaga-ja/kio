//! Raster allocation layout of locked zune-jpeg 0.5.15 (image 0.25.10).
//! Decoder-owned encoded copies and retained APP metadata share the 64MiB limit.
use super::{MAX_IMAGE_DECODED_BYTES, validate_decoded_dimensions};
use kio_core::{KioError, Result};

pub(super) fn validate_jpeg_allocation(bytes: &[u8]) -> Result<()> {
    let err = || KioError::schema("JPEG allocation preflight rejected invalid layout");
    if !bytes.starts_with(&[255, 216]) {
        return Err(err());
    }
    let metadata = super::budget_jpeg_metadata::metadata_budget(bytes)?;
    let encoded = bytes.len() as u64;
    let mut pos = 2;
    let mut layout = None;
    loop {
        if bytes.get(pos) != Some(&255) {
            return Err(err());
        }
        while bytes.get(pos) == Some(&255) {
            pos += 1;
        }
        let marker = *bytes.get(pos).ok_or_else(err)?;
        pos += 1;
        if marker == 217 {
            return Err(err());
        }
        if marker == 1 || (208..=215).contains(&marker) {
            continue;
        }
        let length = bytes.get(pos..pos + 2).ok_or_else(err)?;
        let length = u16::from_be_bytes([length[0], length[1]]) as usize;
        if length < 2 {
            return Err(err());
        }
        let end = pos.checked_add(length).ok_or_else(err)?;
        let segment = bytes.get(pos + 2..end).ok_or_else(err)?;
        pos = end;
        if matches!(marker, 192..=195 | 197..=199 | 201..=203 | 205..=207) {
            // These are the Huffman DCT routes supported by the locked decoder.
            if !matches!(marker, 192..=194)
                || layout.is_some()
                || segment.len() < 6
                || segment[0] != 8
            {
                return Err(err());
            }
            let height = u16::from_be_bytes([segment[1], segment[2]]) as u32;
            let width = u16::from_be_bytes([segment[3], segment[4]]) as u32;
            validate_decoded_dimensions(width, height)?;
            let count = segment[5] as usize;
            if !(1..=4).contains(&count) || segment.len() != 6 + 3 * count {
                return Err(err());
            }
            let mut factors = Vec::with_capacity(count);
            for component in segment[6..].as_chunks::<3>().0 {
                let h = u64::from(component[1] >> 4);
                let v = u64::from(component[1] & 15);
                if !matches!(h, 1 | 2 | 4) || !(1..=4).contains(&v) {
                    return Err(err());
                }
                factors.push((h, v));
            }
            layout = Some((width, height, factors, marker == 194));
        }
        if marker == 218 {
            let (width, height, factors, progressive) = layout.ok_or_else(err)?;
            let count = factors.len();
            if segment.len() < 1 + count.min(1) * 2 + 3 {
                return Err(err());
            }
            let scans = usize::from(segment[0]);
            if scans == 0 || scans > count || segment.len() != 1 + 2 * scans + 3 {
                return Err(err());
            }
            let hmax = factors.iter().map(|v| v.0).max().unwrap();
            let vmax = factors.iter().map(|v| v.1).max().unwrap();
            let mx = u64::from(width).div_ceil(8 * hmax);
            let my = u64::from(height).div_ceil(8 * vmax);
            let interleaved = hmax != 1 || vmax != 1;
            let mut scratch = 0;
            let mut max_stride = 0;
            let mut first_stride = 0;
            let mut row = 0;
            let mut sum_hv = 0;
            let y_bug = factors[0].0 == 2 || factors[0].1 == 2;
            for (i, &(h, v)) in factors.iter().enumerate() {
                let stride = 8 * mx * h;
                max_stride = max_stride.max(stride);
                if i == 0 {
                    first_stride = stride;
                }
                if interleaved && !(count == 1 && progressive) {
                    let ratio = (hmax / h) * (vmax / v);
                    let fix = if i != 0 && y_bug && (h != 1 || v != 1) {
                        2
                    } else {
                        1
                    };
                    scratch += 2 * stride * (2 * v + v * ratio + 8 * ratio * fix);
                }
                sum_hv += h * v;
                row += 16 * stride * v;
            }
            // Grayscale reset_params normalizes sampling after upsample buffers
            // were set up. Preserve those earlier buffers in the live bound.
            let (plane_mx, plane_my) = if count == 1 && interleaved {
                row = 128 * u64::from(width).div_ceil(8);
                sum_hv = 1;
                (u64::from(width).div_ceil(8), u64::from(height).div_ceil(8))
            } else {
                (mx, my)
            };
            if interleaved && count != 1 {
                scratch += if progressive {
                    2 * first_stride
                } else {
                    16 * max_stride
                };
            }
            if !progressive && scans != count && interleaved && count != 1 {
                // finish_baseline_decoding adds its scratch while the caller
                // retains the original baseline scratch vector.
                scratch += 2 * first_stride;
            }
            if progressive || scans != count {
                scratch += 128 * plane_mx * plane_my * sum_hv;
            }
            let output_channels = if count == 1 { 1 } else { 3 };
            let output = u64::from(width) * u64::from(height) * output_channels;
            // The wrapper's encoded copy lives through both header passes.
            // Header replacement/sort/concat peaks precede MCU arrays; later
            // metadata markers can overlap the retained coefficient arrays.
            let header_peak = encoded + output + metadata.header_peak;
            let scan_peak = encoded + output + row + scratch + metadata.scan_peak;
            if header_peak.max(scan_peak) > MAX_IMAGE_DECODED_BYTES {
                return Err(KioError::schema(
                    "JPEG decoded allocation exceeds the supported limit",
                ));
            }
            return Ok(());
        }
    }
}
