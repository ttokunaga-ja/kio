//! Allocation preflight for the lossless streams consumed by locked image-webp 0.2.4.
//! Huffman descriptions are read before the native decoder can allocate their tables.
use kio_core::{KioError, Result};

const LIMIT: u64 = 64 * 1024 * 1024;
// Includes parser scratch and decoder bookkeeping/code-length scratch. The locked
// decoder's largest temporary alphabet is 2328 u16s. No input-sized parser table.
const SCRATCH: u64 = 64 * 1024;
const GROUP: u64 = 5 * 56;
const ORDER: [usize; 19] = [
    17, 18, 0, 1, 2, 3, 4, 5, 16, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
];

fn invalid() -> KioError {
    KioError::schema("WebP lossless allocation preflight rejected input")
}
fn check(bytes: u64) -> Result<()> {
    if bytes > LIMIT {
        Err(KioError::schema("WebP decoder allocation exceeds 64 MiB"))
    } else {
        Ok(())
    }
}
fn le24(v: &[u8]) -> usize {
    usize::from(v[0]) | usize::from(v[1]) << 8 | usize::from(v[2]) << 16
}

pub(super) fn validate_webp_allocation(bytes: &[u8], decoded_bytes: u64) -> Result<()> {
    if bytes.get(..4) != Some(b"RIFF") || bytes.get(8..12) != Some(b"WEBP") {
        return Err(invalid());
    }
    inspect_chunks(&bytes[12..], decoded_bytes + SCRATCH, None, false)
}

fn inspect_chunks(
    mut bytes: &[u8],
    base: u64,
    dimensions: Option<(usize, usize)>,
    animation_frame: bool,
) -> Result<()> {
    let mut dimensions = dimensions;
    let mut alpha_pixels = 0u64;
    let mut output_alpha = None;
    while !bytes.is_empty() {
        let header = bytes.get(..8).ok_or_else(invalid)?;
        let size = u32::from_le_bytes(header[4..8].try_into().map_err(|_| invalid())?) as usize;
        let payload = bytes
            .get(8..8usize.checked_add(size).ok_or_else(invalid)?)
            .ok_or_else(invalid)?;
        match &header[..4] {
            b"VP8X" => {
                let p = payload.get(..10).ok_or_else(invalid)?;
                dimensions = Some((le24(&p[4..7]) + 1, le24(&p[7..10]) + 1));
                output_alpha = Some(p[0] & 0x10 != 0);
            }
            b"VP8L" => {
                let mut parser = Parser {
                    bits: Bits {
                        bytes: payload,
                        position: 0,
                    },
                    base,
                };
                if parser.bits.read(8)? != 0x2f {
                    return Err(invalid());
                }
                let width = parser.bits.read(14)? as usize + 1;
                let height = parser.bits.read(14)? as usize + 1;
                let alpha = parser.bits.read(1)? != 0;
                if parser.bits.read(3)? != 0 {
                    return Err(invalid());
                }
                // Animation always decodes into a separate RGBA frame. Static RGB
                // output also requires a separate RGBA lossless working image.
                if animation_frame || !output_alpha.unwrap_or(alpha) {
                    parser.base += (width * height * 4) as u64;
                }
                parser.frame(width, height)?;
                return Ok(());
            }
            b"ALPH" if animation_frame || output_alpha != Some(false) => {
                let flags = *payload.first().ok_or_else(invalid)?;
                let (width, height) = dimensions.ok_or_else(invalid)?;
                alpha_pixels = (width * height) as u64;
                let retained_yuv = if animation_frame {
                    0
                } else {
                    (width.div_ceil(16) * height.div_ceil(16) * 384) as u64
                };
                check(base + retained_yuv + alpha_pixels)?;
                if flags & 3 == 1 {
                    if width > 65535 || height > 65535 {
                        return Err(invalid());
                    }
                    let mut parser = Parser {
                        bits: Bits {
                            bytes: &payload[1..],
                            position: 0,
                        },
                        base: base + (width * height * 5) as u64 + retained_yuv,
                    };
                    parser.frame(width, height)?;
                }
            }
            b"VP8 " => {
                let (width, height, peak) =
                    super::budget_vp8::allocation_bound(payload).map_err(KioError::schema)?;
                let pixels = u64::from(width) * u64::from(height);
                // Static alpha is decoded after VP8's temporary decoder drops;
                // animated alpha is decoded first and retained during VP8.
                let retained_alpha = if animation_frame { alpha_pixels } else { 0 };
                check(base + retained_alpha + peak)?;
                if animation_frame {
                    let frame = super::budget_vp8::frame_storage(width, height)
                        .map_err(KioError::schema)?;
                    let channels = if alpha_pixels == 0 { 3 } else { 4 };
                    check(base + retained_alpha + frame + pixels * channels)?;
                }
            }
            b"ANMF" => {
                let p = payload.get(..16).ok_or_else(invalid)?;
                let width = le24(&p[6..9]) + 1;
                let height = le24(&p[9..12]) + 1;
                // Native animation holds the output canvas, its compositing canvas,
                // and an RGBA frame concurrently. Only frame zero is consumed.
                check(base + base.saturating_sub(SCRATCH) + (width * height * 4) as u64)?;
                return inspect_chunks(&payload[16..], base, Some((width, height)), true);
            }
            _ => {}
        }
        let end = 8usize
            .checked_add(size)
            .and_then(|v| v.checked_add(size & 1))
            .ok_or_else(invalid)?;
        bytes = bytes.get(end..).ok_or_else(invalid)?;
    }
    Ok(())
}

struct Bits<'a> {
    bytes: &'a [u8],
    position: usize,
}
impl Bits<'_> {
    fn read(&mut self, count: usize) -> Result<u32> {
        if count > 24
            || self
                .position
                .checked_add(count)
                .is_none_or(|end| end > self.bytes.len() * 8)
        {
            return Err(invalid());
        }
        let mut value = 0;
        for bit in 0..count {
            value |= u32::from((self.bytes[self.position / 8] >> (self.position % 8)) & 1) << bit;
            self.position += 1;
        }
        Ok(value)
    }
}

struct Tree {
    counts: [u16; 16],
    symbols: Vec<u16>,
    single: Option<u16>,
    allocation: u64,
}
impl Tree {
    fn lengths(lengths: &[u16]) -> Result<Self> {
        let mut counts = [0u16; 16];
        for &length in lengths {
            if length > 15 {
                return Err(invalid());
            }
            counts[usize::from(length)] += 1;
        }
        let n = lengths.len() - usize::from(counts[0]);
        if n == 0 {
            return Err(invalid());
        }
        if n == 1 {
            return Ok(Self::single(
                lengths.iter().position(|&v| v != 0).ok_or_else(invalid)? as u16,
            ));
        }
        let mut remaining = 1i32;
        for &count in &counts[1..] {
            remaining = remaining * 2 - i32::from(count);
            if remaining < 0 {
                return Err(invalid());
            }
        }
        if remaining != 0 {
            return Err(invalid());
        }
        let max = counts.iter().rposition(|&v| v != 0).ok_or_else(invalid)?;
        let table_bits = max.min(10);
        // HuffmanTreeNode is at most 16 bytes on supported 32/64-bit targets;
        // implicit trees reserve exactly twice the long-code symbol count.
        let long: u64 = counts[table_bits + 1..].iter().map(|&v| u64::from(v)).sum();
        let allocation = (4u64 << table_bits) + 32 * long;
        let mut symbols = Vec::with_capacity(n);
        for length in 1..=15 {
            for (symbol, &v) in lengths.iter().enumerate() {
                if v == length {
                    symbols.push(symbol as u16);
                }
            }
        }
        counts[0] = 0;
        Ok(Self {
            counts,
            symbols,
            single: None,
            allocation,
        })
    }
    fn single(symbol: u16) -> Self {
        Self {
            counts: [0; 16],
            symbols: Vec::new(),
            single: Some(symbol),
            allocation: 0,
        }
    }
    fn read(&self, bits: &mut Bits<'_>) -> Result<u16> {
        if let Some(symbol) = self.single {
            return Ok(symbol);
        }
        let (mut code, mut first, mut offset) = (0u32, 0u32, 0usize);
        for length in 1..=15 {
            code = code * 2 + bits.read(1)?;
            let count = u32::from(self.counts[length]);
            if code >= first && code - first < count {
                return self
                    .symbols
                    .get(offset + (code - first) as usize)
                    .copied()
                    .ok_or_else(invalid);
            }
            offset += count as usize;
            first = (first + count) * 2;
        }
        Err(invalid())
    }
}

struct Parser<'a> {
    bits: Bits<'a>,
    base: u64,
}
impl Parser<'_> {
    fn tree(&mut self, alphabet: usize) -> Result<Tree> {
        if self.bits.read(1)? != 0 {
            let two = self.bits.read(1)? != 0;
            let wide = self.bits.read(1)? != 0;
            let zero = self.bits.read(if wide { 8 } else { 1 })? as u16;
            if usize::from(zero) >= alphabet {
                return Err(invalid());
            }
            if !two {
                return Ok(Tree::single(zero));
            }
            let one = self.bits.read(8)? as u16;
            if usize::from(one) >= alphabet {
                return Err(invalid());
            }
            // The locked decoder permits duplicate simple symbols as well.
            return Ok(Tree {
                counts: {
                    let mut c = [0; 16];
                    c[1] = 2;
                    c
                },
                symbols: vec![zero, one],
                single: None,
                allocation: 56,
            });
        }
        let mut lengths = [0u16; 19];
        let count = self.bits.read(4)? as usize + 4;
        for &symbol in &ORDER[..count] {
            lengths[symbol] = self.bits.read(3)? as u16;
        }
        let code_tree = Tree::lengths(&lengths)?;
        let mut max = if self.bits.read(1)? != 0 {
            let nbits = 2 + 2 * self.bits.read(3)? as usize;
            let value = self.bits.read(nbits)? as usize + 2;
            if value > alphabet {
                return Err(invalid());
            }
            value
        } else {
            alphabet
        };
        let mut lengths = vec![0u16; alphabet];
        let (mut symbol, mut previous) = (0, 8);
        while symbol < alphabet && max != 0 {
            max -= 1;
            let code = code_tree.read(&mut self.bits)?;
            if code < 16 {
                lengths[symbol] = code;
                if code != 0 {
                    previous = code;
                }
                symbol += 1;
            } else {
                let (extra, offset) = match code {
                    16 => (2, 3),
                    17 => (3, 3),
                    18 => (7, 11),
                    _ => return Err(invalid()),
                };
                let repeat = self.bits.read(extra)? as usize + offset;
                let end = symbol
                    .checked_add(repeat)
                    .filter(|&v| v <= alphabet)
                    .ok_or_else(invalid)?;
                lengths[symbol..end].fill(if code == 16 { previous } else { 0 });
                symbol = end;
            }
        }
        Tree::lengths(&lengths)
    }
    fn frame(&mut self, mut width: usize, height: usize) -> Result<()> {
        let mut seen = 0u32;
        while self.bits.read(1)? != 0 {
            let transform = self.bits.read(2)?;
            if seen & (1 << transform) != 0 {
                return Err(invalid());
            }
            seen |= 1 << transform;
            match transform {
                0 | 1 => {
                    let bits = self.bits.read(3)? as usize + 2;
                    let w = width.div_ceil(1 << bits);
                    let h = height.div_ceil(1 << bits);
                    self.base += (w * h * 4) as u64;
                    self.stream(w, h, false)?;
                }
                2 => {}
                3 => {
                    let colors = self.bits.read(8)? as usize + 1;
                    self.base += (colors * 4) as u64;
                    self.stream(colors, 1, false)?;
                    width = width.div_ceil(if colors <= 2 {
                        8
                    } else if colors <= 4 {
                        4
                    } else if colors <= 16 {
                        2
                    } else {
                        1
                    });
                }
                _ => return Err(invalid()),
            }
        }
        self.stream(width, height, true)?;
        Ok(())
    }
    fn stream(&mut self, width: usize, height: usize, main: bool) -> Result<Vec<[u8; 4]>> {
        check(self.base)?;
        let cache_bits = if self.bits.read(1)? != 0 {
            let bits = self.bits.read(4)? as usize;
            if !(1..=11).contains(&bits) {
                return Err(invalid());
            }
            bits
        } else {
            0
        };
        let cache_bytes = if cache_bits == 0 {
            0
        } else {
            4u64 << cache_bits
        };
        let mut groups = 1usize;
        let mut entropy_bytes = 0;
        if main && self.bits.read(1)? != 0 {
            let bits = self.bits.read(3)? as usize + 2;
            let w = width.div_ceil(1 << bits);
            let h = height.div_ceil(1 << bits);
            let temporary = (w * h * 4) as u64;
            self.base += temporary + cache_bytes;
            let data = self.stream(w, h, false)?;
            self.base -= temporary + cache_bytes;
            for pixel in data {
                groups = groups.max((usize::from(pixel[0]) << 8 | usize::from(pixel[1])) + 1);
            }
            entropy_bytes = (w * h * 2) as u64;
            check(self.base + cache_bytes + temporary + entropy_bytes)?;
        }
        let base = self.base + cache_bytes + entropy_bytes;
        let (mut capacity, mut dynamic) = (0usize, 0u64);
        let mut retained = Vec::new();
        for group in 0..groups {
            for channel in 0..5 {
                let alphabet = match channel {
                    0 => 280 + if cache_bits == 0 { 0 } else { 1 << cache_bits },
                    4 => 40,
                    _ => 256,
                };
                let tree = self.tree(alphabet)?;
                dynamic += tree.allocation;
                check(base + capacity as u64 * GROUP + dynamic)?;
                if !main {
                    retained.push(tree);
                }
            }
            if group == capacity {
                let next = if capacity == 0 { 4 } else { capacity * 2 };
                check(base + (capacity + next) as u64 * GROUP + dynamic)?;
                capacity = next;
            }
        }
        // Main image pixels are left for the native decoder. Only embedded image
        // pixels must be decoded here to locate the next Huffman descriptions.
        if main {
            return Ok(Vec::new());
        }
        decode_subimage(&mut self.bits, width, height, &retained, cache_bits)
    }
}

fn prefix(bits: &mut Bits<'_>, code: u16) -> Result<usize> {
    if code < 4 {
        return Ok(usize::from(code) + 1);
    }
    let extra = usize::from((code - 2) >> 1);
    Ok(((2 + (usize::from(code) & 1)) << extra) + bits.read(extra)? as usize + 1)
}

fn decode_subimage(
    bits: &mut Bits<'_>,
    width: usize,
    height: usize,
    trees: &[Tree],
    cache_bits: usize,
) -> Result<Vec<[u8; 4]>> {
    let count = width.checked_mul(height).ok_or_else(invalid)?;
    check((count * 4) as u64 + SCRATCH)?;
    let mut pixels = vec![[0u8; 4]; count];
    let mut cache = vec![[0u8; 4]; if cache_bits == 0 { 0 } else { 1 << cache_bits }];
    let mut index = 0;
    while index < count {
        let green = trees[0].read(bits)?;
        if green < 256 {
            pixels[index] = [
                trees[1].read(bits)? as u8,
                green as u8,
                trees[2].read(bits)? as u8,
                trees[3].read(bits)? as u8,
            ];
            insert_cache(&mut cache, pixels[index], cache_bits);
            index += 1;
        } else if green < 280 {
            let length = prefix(bits, green - 256)?;
            let distance_symbol = trees[4].read(bits)?;
            let distance = prefix(bits, distance_symbol)?;
            let distance = if distance > 120 {
                distance - 120
            } else {
                let (x, y) = DISTANCES[distance - 1];
                (isize::from(x) + isize::from(y) * width as isize).max(1) as usize
            };
            if distance > index || length > count - index {
                return Err(invalid());
            }
            for _ in 0..length {
                pixels[index] = pixels[index - distance];
                // image-webp's distance-one branch omits cache updates.
                if distance != 1 {
                    insert_cache(&mut cache, pixels[index], cache_bits);
                }
                index += 1;
            }
        } else {
            pixels[index] = *cache.get(usize::from(green - 280)).ok_or_else(invalid)?;
            index += 1;
        }
    }
    Ok(pixels)
}
fn insert_cache(cache: &mut [[u8; 4]], pixel: [u8; 4], bits: usize) {
    if bits == 0 {
        return;
    }
    let [r, g, b, a] = pixel;
    let argb = u32::from(a) << 24 | u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b);
    let index = argb.wrapping_mul(0x1e35a7bd) >> (32 - bits);
    cache[index as usize] = pixel;
}

// VP8L plane-code offsets from the lossless bitstream distance mapping.
#[rustfmt::skip]
const DISTANCES: [(i8, i8); 120] = [
    (0, 1),  (1, 0),  (1, 1),  (-1, 1), (0, 2),  (2, 0),  (1, 2),  (-1, 2),
    (2, 1),  (-2, 1), (2, 2),  (-2, 2), (0, 3),  (3, 0),  (1, 3),  (-1, 3),
    (3, 1),  (-3, 1), (2, 3),  (-2, 3), (3, 2),  (-3, 2), (0, 4),  (4, 0),
    (1, 4),  (-1, 4), (4, 1),  (-4, 1), (3, 3),  (-3, 3), (2, 4),  (-2, 4),
    (4, 2),  (-4, 2), (0, 5),  (3, 4),  (-3, 4), (4, 3),  (-4, 3), (5, 0),
    (1, 5),  (-1, 5), (5, 1),  (-5, 1), (2, 5),  (-2, 5), (5, 2),  (-5, 2),
    (4, 4),  (-4, 4), (3, 5),  (-3, 5), (5, 3),  (-5, 3), (0, 6),  (6, 0),
    (1, 6),  (-1, 6), (6, 1),  (-6, 1), (2, 6),  (-2, 6), (6, 2),  (-6, 2),
    (4, 5),  (-4, 5), (5, 4),  (-5, 4), (3, 6),  (-3, 6), (6, 3),  (-6, 3),
    (0, 7),  (7, 0),  (1, 7),  (-1, 7), (5, 5),  (-5, 5), (7, 1),  (-7, 1),
    (4, 6),  (-4, 6), (6, 4),  (-6, 4), (2, 7),  (-2, 7), (7, 2),  (-7, 2),
    (3, 7),  (-3, 7), (7, 3),  (-7, 3), (5, 6),  (-5, 6), (6, 5),  (-6, 5),
    (8, 0),  (4, 7),  (-4, 7), (7, 4),  (-7, 4), (8, 1),  (8, 2),  (6, 6),
    (-6, 6), (8, 3),  (5, 7),  (-5, 7), (7, 5),  (-7, 5), (8, 4),  (6, 7),
    (-6, 7), (7, 6),  (-7, 6), (8, 5),  (7, 7),  (-7, 7), (8, 6),  (8, 7)
];

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Writer {
        bytes: Vec<u8>,
        position: usize,
    }
    impl Writer {
        fn put(&mut self, value: u32, count: usize) {
            for bit in 0..count {
                if self.position.is_multiple_of(8) {
                    self.bytes.push(0);
                }
                self.bytes[self.position / 8] |=
                    (((value >> bit) & 1) as u8) << (self.position % 8);
                self.position += 1;
            }
        }
        fn single(&mut self, symbol: u32) {
            self.put(1, 1);
            self.put(0, 1);
            self.put(u32::from(symbol > 1), 1);
            self.put(symbol, if symbol > 1 { 8 } else { 1 });
        }
    }
    fn groups_fixture(lookup: bool) -> Vec<u8> {
        let mut writer = Writer::default();
        writer.put(0x2f, 8);
        writer.put(0, 14);
        writer.put(0, 14);
        writer.put(1, 1);
        writer.put(0, 3);
        writer.put(0, 1); // no transforms
        writer.put(0, 1); // no cache
        writer.put(1, 1);
        writer.put(0, 3); // one-pixel entropy image
        writer.put(0, 1); // entropy cache
        for symbol in [255, 255, 0, 255, 0] {
            writer.single(symbol);
        }
        for _ in 0..65536 {
            if lookup {
                writer.put(0, 1);
                writer.put(8, 4);
                for symbol in [17, 18, 0, 1, 2, 3, 4, 5, 16, 6, 7, 8] {
                    writer.put(u32::from(symbol == 0 || symbol == 8), 3);
                }
                writer.put(0, 1);
                for _ in 0..256 {
                    writer.put(1, 1);
                }
                for _ in 0..24 {
                    writer.put(0, 1);
                }
            } else {
                writer.single(0);
            }
            for symbol in [0, 0, 255, 0] {
                writer.single(symbol);
            }
        }
        if lookup {
            writer.put(0, 8);
        }
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(
            &(12 + writer.bytes.len() as u32 + writer.bytes.len() as u32 % 2).to_le_bytes(),
        );
        bytes.extend_from_slice(b"WEBPVP8L");
        bytes.extend_from_slice(&(writer.bytes.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&writer.bytes);
        if writer.bytes.len() % 2 != 0 {
            bytes.push(0);
        }
        bytes
    }

    #[test]
    fn actual_huffman_shapes_distinguish_safe_and_overbudget_group_counts() {
        // Same one-pixel output and 65536 groups. Single-symbol trees request
        // 27,525,434 bytes at native peak; green lookup tables request 85,459,818.
        assert!(validate_webp_allocation(&groups_fixture(false), 4).is_ok());
        assert!(validate_webp_allocation(&groups_fixture(true), 4).is_err());
    }

    #[test]
    fn canonical_reader_and_native_table_budget_follow_code_lengths() {
        let tree = Tree::lengths(&[1, 2, 3, 3]).unwrap();
        let mut bits = Bits {
            bytes: &[0b1101_1010],
            position: 0,
        };
        assert_eq!(tree.read(&mut bits).unwrap(), 0); // 0
        assert_eq!(tree.read(&mut bits).unwrap(), 1); // 10
        assert_eq!(tree.read(&mut bits).unwrap(), 2); // 110
        assert_eq!(tree.allocation, 32);
        assert!(Tree::lengths(&[1, 1, 1]).is_err());
    }

    #[test]
    fn malformed_and_truncated_descriptions_fail_closed() {
        assert!(validate_webp_allocation(b"RIFF", 4).is_err());
        let mut bytes = groups_fixture(false);
        bytes.truncate(bytes.len() / 2);
        assert!(validate_webp_allocation(&bytes, 4).is_err());
        assert!(check(LIMIT + 1).is_err());
        assert!(check(LIMIT).is_ok());
    }
}
