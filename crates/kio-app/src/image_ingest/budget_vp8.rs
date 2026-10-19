//! Heap bound for the locked image-webp 0.2.4 keyframe decoder.
//! Caller-owned output, alpha and animation canvas storage is added by WebP routing.

const FIXED: u64 = 64 * 1024;

fn invalid() -> String {
    "WebP VP8 allocation preflight rejected input".into()
}

pub(super) fn frame_storage(width: u32, height: u32) -> Result<u64, String> {
    if width == 0 || height == 0 || width > 8192 || height > 8192 {
        return Err(invalid());
    }
    let pixels = u64::from(width) * u64::from(height);
    if pixels > 16_000_000 {
        return Err(invalid());
    }
    Ok(u64::from(width.div_ceil(16)) * u64::from(height.div_ceil(16)) * 384)
}

pub(super) fn allocation_bound(vp8: &[u8]) -> Result<(u32, u32, u64), String> {
    let header = vp8.get(..10).ok_or_else(invalid)?;
    let tag = u32::from(header[0]) | u32::from(header[1]) << 8 | u32::from(header[2]) << 16;
    if tag & 1 != 0 || header[3..6] != [0x9d, 1, 0x2a] {
        return Err(invalid());
    }
    let width = u32::from(u16::from_le_bytes([header[6], header[7]]) & 0x3fff);
    let height = u32::from(u16::from_le_bytes([header[8], header[9]]) & 0x3fff);
    let yuv = frame_storage(width, height)?;
    let end = 10usize
        .checked_add((tag >> 5) as usize)
        .ok_or_else(invalid)?;
    let first = vp8.get(10..end).ok_or_else(invalid)?;
    let partitions = partition_count(first)?;
    let sizes_end = end.checked_add(3 * (partitions - 1)).ok_or_else(invalid)?;
    let sizes = vp8.get(end..sizes_end).ok_or_else(invalid)?;
    let mut remaining = vp8.len() - sizes_end;
    for size in sizes.as_chunks::<3>().0 {
        let size = usize::from(size[0]) | usize::from(size[1]) << 8 | usize::from(size[2]) << 16;
        remaining = remaining.checked_sub(size).ok_or_else(invalid)?;
    }
    let mw = u64::from(width.div_ceil(16));
    let blocks = mw * u64::from(height.div_ceil(16));
    // MacroBlock is 30 bytes in the locked source. Reserve 64 per element,
    // including enum/layout slack. Vec push doubles capacity: old+new at a
    // reallocation is <=3*N (minimum initial capacity four). Include both
    // allocations rather than assuming realloc can extend in place.
    let macroblocks = (3 * blocks).max(4) * 64 + mw * 64;
    let borders = u64::from(width) + 20 + mw * 16 + 35 + 17;
    // All partition sizes have been checked against input before native reads.
    // Four payload lengths cover rounded arithmetic chunks, read_to_end Vec
    // growth and the transient copy into boxed chunks, including previous
    // partitions. 128 covers rounding, the partition-size table and tiny Vecs.
    let encoded_scratch = 4 * vp8.len() as u64 + 128;
    Ok((
        width,
        height,
        yuv + macroblocks + borders + encoded_scratch + FIXED,
    ))
}

// Only the bool-coded header preceding the partition count is inspected. All
// these fields use probability 128; no entropy/image-sized tables are needed.
fn partition_count(bytes: &[u8]) -> Result<usize, String> {
    let mut bits = BoolBits {
        bytes,
        position: 0,
        value: 0,
        range: 255,
        count: -8,
    };
    if bits.literal(1)? != 0 {
        return Err(invalid());
    }
    bits.literal(1)?; // clamping
    if bits.flag()? {
        // segmentation
        let map = bits.flag()?;
        if bits.flag()? {
            bits.flag()?;
            for _ in 0..4 {
                bits.optional_signed(7)?;
            }
            for _ in 0..4 {
                bits.optional_signed(6)?;
            }
        }
        if map {
            for _ in 0..3 {
                if bits.flag()? {
                    bits.literal(8)?;
                }
            }
        }
    }
    bits.literal(1)?; // filter type
    bits.literal(6)?; // filter level
    bits.literal(3)?; // sharpness
    if bits.flag()? && bits.flag()? {
        for _ in 0..8 {
            bits.optional_signed(6)?;
        }
    }
    Ok(1usize << bits.literal(2)?)
}

struct BoolBits<'a> {
    bytes: &'a [u8],
    position: usize,
    value: u32,
    range: u32,
    count: i32,
}

impl BoolBits<'_> {
    fn flag(&mut self) -> Result<bool, String> {
        if self.count < 0 {
            // The locked decoder permits one zero byte past EOF.
            let byte = if self.position == self.bytes.len() {
                0
            } else {
                *self.bytes.get(self.position).ok_or_else(invalid)?
            };
            self.position += 1;
            self.value = (self.value << 8) | u32::from(byte);
            self.count += 8;
        }
        let split = 1 + ((self.range - 1) >> 1);
        let threshold = split << self.count;
        let bit = self.value >= threshold;
        if bit {
            self.value -= threshold;
            self.range -= split;
        } else {
            self.range = split;
        }
        let shift = self.range.leading_zeros().saturating_sub(24);
        self.range <<= shift;
        self.count -= shift as i32;
        Ok(bit)
    }
    fn literal(&mut self, count: usize) -> Result<u32, String> {
        let mut value = 0;
        for _ in 0..count {
            value = (value << 1) | u32::from(self.flag()?);
        }
        Ok(value)
    }
    fn optional_signed(&mut self, count: usize) -> Result<(), String> {
        if self.flag()? {
            self.literal(count)?;
            self.flag()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Real cwebp keyframe, solid gray 32x32, quality 10.
    const SMALL: &[u8] = &[
        240, 2, 0, 157, 1, 42, 32, 0, 32, 0, 63, 113, 184, 215, 98, 52, 175, 44, 166, 40, 8, 2,
        144, 46, 9, 105, 0, 0, 72, 193, 136, 0, 0, 0, 0, 0, 0, 0,
    ];

    #[test]
    fn accepts_small_lossy_and_accounts_internal_storage() {
        let (w, h, allocation) = allocation_bound(SMALL).unwrap();
        assert_eq!((w, h), (32, 32));
        assert!(allocation > frame_storage(w, h).unwrap());
        assert!(allocation + 4 * u64::from(w) * u64::from(h) < 64 * 1024 * 1024);
    }
    #[test]
    fn full_size_lossy_exceeds_budget_before_decoder() {
        let mut bytes = SMALL.to_vec();
        bytes[6..8].copy_from_slice(&4000u16.to_le_bytes());
        bytes[8..10].copy_from_slice(&4000u16.to_le_bytes());
        let (_, _, allocation) = allocation_bound(&bytes).unwrap();
        assert!(allocation + 3 * 4000 * 4000 > 64 * 1024 * 1024);
    }
    #[test]
    fn rejects_truncated_first_partition_and_invalid_dimensions() {
        assert!(allocation_bound(&SMALL[..20]).is_err());
        assert!(frame_storage(8193, 1).is_err());
        assert!(frame_storage(8192, 8192).is_err());
        assert!(frame_storage(0, 1).is_err());
    }
}
