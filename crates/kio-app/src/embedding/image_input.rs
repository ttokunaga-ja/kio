//! Canonical image bytes submitted to every embedding runtime.
use kio_core::Result;
use kio_core::cas::ImageInputReceipt;

pub(crate) struct PreparedEmbeddingImage {
    pub(crate) bytes: Vec<u8>,
    pub(crate) mime: &'static str,
    pub(crate) receipt: Option<ImageInputReceipt>,
}

pub(crate) fn prepare_embedding_image(
    source_hash: &str,
    bytes: Vec<u8>,
    mime: &str,
) -> Result<PreparedEmbeddingImage> {
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};
    use image::{ExtendedColorType, ImageEncoder};
    use kio_core::{KioError, cas::is_hash};
    use kio_pipeline::prepare::hash_bytes;
    if !is_hash(source_hash) || hash_bytes(&bytes) != source_hash {
        return Err(KioError::schema("embedding source image identity mismatch"));
    }
    match mime {
        "image/png" | "image/jpeg" => {
            crate::image_ingest::validate_source_image_payload(&bytes, mime)?;
            return Ok(PreparedEmbeddingImage {
                bytes,
                mime: if mime == "image/png" {
                    "image/png"
                } else {
                    "image/jpeg"
                },
                receipt: None,
            });
        }
        "image/gif" | "image/webp" => (),
        _ => {
            return Err(KioError::schema(
                "embedding source image media type is unsupported",
            ));
        }
    }
    let (mut rgba, width, height, orientation) =
        crate::image_ingest::decode_embedding_rgba(&bytes, mime)?;
    let (width, height) = orient_rgba(&mut rgba, width, height, orientation);
    for pixel in rgba.as_chunks_mut::<4>().0 {
        if pixel[3] == 0 {
            pixel[..3].fill(0);
        }
    }
    let mut output = CappedPng(Vec::new());
    PngEncoder::new_with_quality(&mut output, CompressionType::Fast, FilterType::NoFilter)
        .write_image(&rgba, width, height, ExtendedColorType::Rgba8)
        .map_err(|error| {
            KioError::schema(format!("canonical image PNG encoding failed: {error}"))
        })?;
    let receipt = ImageInputReceipt {
        source_hash: source_hash.to_owned(),
        source_mime: mime.to_owned(),
        sent_hash: hash_bytes(&output.0),
        sent_mime: "image/png".to_owned(),
        transcoder_pin: transcoder_pin(),
    };
    receipt.validate()?;
    Ok(PreparedEmbeddingImage {
        bytes: output.0,
        mime: "image/png",
        receipt: Some(receipt),
    })
}

/// The pin includes the exact locked dependency records, rather than a human
/// version label that could silently remain unchanged after a codec update.
fn transcoder_pin() -> String {
    use kio_pipeline::prepare::hash_bytes;
    const LOCK: &str = include_str!("../../../../Cargo.lock");
    let names = [
        "image",
        "image-webp",
        "gif",
        "weezl",
        "png",
        "fdeflate",
        "flate2",
        "miniz_oxide",
        "simd-adler32",
        "crc32fast",
        "adler2",
        "byteorder-lite",
        "bytemuck",
        "num-traits",
        "bitflags",
        "cfg-if",
    ];
    let mut recipe = String::from(
        "kio_image_png_v1;frame0;transparent-fullcanvas;rgba8-straight;alpha0-rgb0;webp-exif;no-color-transform;png-fast-no-filter;no-metadata\n",
    );
    for block in LOCK.split("[[package]]").skip(1) {
        let name = block
            .lines()
            .find_map(|line| line.strip_prefix("name = "))
            .unwrap_or("")
            .trim_matches('"');
        if names.contains(&name) {
            for line in block.lines().filter(|line| {
                line.starts_with("name = ")
                    || line.starts_with("version = ")
                    || line.starts_with("checksum = ")
            }) {
                recipe.push_str(line);
                recipe.push('\n');
            }
        }
    }
    format!("kio_image_png_v1:{}", hash_bytes(recipe.as_bytes()))
}

/// One-bit visitation keeps a 16M-pixel RGBA canvas and orientation work below
/// 64MiB, without a second full raster for 90-degree rotations/transposes.
fn orient_rgba(rgba: &mut [u8], width: u32, height: u32, orientation: u8) -> (u32, u32) {
    if orientation == 1 {
        return (width, height);
    }
    let w = width as usize;
    let h = height as usize;
    let swapped = (5..=8).contains(&orientation);
    let ow = if swapped { h } else { w };
    let mut seen = vec![0_u8; (w * h).div_ceil(8)];
    for start in 0..w * h {
        if seen[start / 8] & (1 << (start % 8)) != 0 {
            continue;
        }
        let mut held: [u8; 4] = rgba[start * 4..start * 4 + 4].try_into().unwrap();
        let mut current = start;
        loop {
            seen[current / 8] |= 1 << (current % 8);
            let (x, y) = (current % w, current / w);
            let (nx, ny) = match orientation {
                2 => (w - 1 - x, y),
                3 => (w - 1 - x, h - 1 - y),
                4 => (x, h - 1 - y),
                5 => (y, x),
                6 => (h - 1 - y, x),
                7 => (h - 1 - y, w - 1 - x),
                8 => (y, w - 1 - x),
                _ => (x, y),
            };
            let next = ny * ow + nx;
            for (channel, value) in held.iter_mut().enumerate() {
                std::mem::swap(value, &mut rgba[next * 4 + channel]);
            }
            current = next;
            if current == start {
                break;
            }
        }
    }
    if swapped {
        (height, width)
    } else {
        (width, height)
    }
}

struct CappedPng(Vec<u8>);
impl std::io::Write for CappedPng {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > kio_adapter::types::MAX_EMBEDDING_ITEM_BYTES.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other(
                "canonical image PNG exceeds the embedding input byte limit",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kio_pipeline::prepare::hash_bytes;
    const FIXTURES: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../kio-cli/tests/fixtures/o2_golden"
    );

    #[test]
    fn o2_canonical_png_goldens_and_receipts() {
        for (source, expected, mime) in [
            (
                "gif-offset-poison-later.gif",
                "gif-offset-frame0.png",
                "image/gif",
            ),
            (
                "webp-offset-poison-later.webp",
                "webp-offset-frame0.png",
                "image/webp",
            ),
            (
                "webp-transparent-rgb.webp",
                "transparent-rgb-zero.png",
                "image/webp",
            ),
            (
                "webp-exif-orientation6.webp",
                "webp-orientation6.png",
                "image/webp",
            ),
        ] {
            let bytes = std::fs::read(format!("{FIXTURES}/{source}")).unwrap();
            let source_hash = hash_bytes(&bytes);
            let prepared = prepare_embedding_image(&source_hash, bytes, mime).unwrap();
            assert_eq!(
                prepared.bytes,
                std::fs::read(format!("{FIXTURES}/{expected}")).unwrap(),
                "{source}"
            );
            assert_eq!(prepared.mime, "image/png");
            let receipt = prepared
                .receipt
                .expect("converted image must carry receipt");
            assert_eq!(receipt.source_hash, source_hash);
            assert_eq!(receipt.source_mime, mime);
            assert_eq!(receipt.sent_hash, hash_bytes(&prepared.bytes));
            assert_eq!(receipt.sent_mime, prepared.mime);
            receipt.validate().unwrap();
        }
    }

    #[test]
    fn o2_all_exif_orientations_preserve_pixels() {
        let source: Vec<u8> = (1..=6).flat_map(|value| [value, 0, 0, 255]).collect();
        for (orientation, expected, dimensions) in [
            (1, [1, 2, 3, 4, 5, 6], (2, 3)),
            (2, [2, 1, 4, 3, 6, 5], (2, 3)),
            (3, [6, 5, 4, 3, 2, 1], (2, 3)),
            (4, [5, 6, 3, 4, 1, 2], (2, 3)),
            (5, [1, 3, 5, 2, 4, 6], (3, 2)),
            (6, [5, 3, 1, 6, 4, 2], (3, 2)),
            (7, [6, 4, 2, 5, 3, 1], (3, 2)),
            (8, [2, 4, 6, 1, 3, 5], (3, 2)),
        ] {
            let mut pixels = source.clone();
            assert_eq!(orient_rgba(&mut pixels, 2, 3, orientation), dimensions);
            assert_eq!(
                pixels
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|pixel| pixel[0])
                    .collect::<Vec<_>>(),
                expected
            );
        }
    }

    #[test]
    fn o2_original_png_jpeg_unchanged_and_later_gif_validated() {
        for (source, mime) in [
            ("png-original-control.png", "image/png"),
            ("jpeg-original-control.jpg", "image/jpeg"),
        ] {
            let bytes = std::fs::read(format!("{FIXTURES}/{source}")).unwrap();
            let prepared =
                prepare_embedding_image(&hash_bytes(&bytes), bytes.clone(), mime).unwrap();
            assert_eq!(prepared.bytes, bytes);
            assert_eq!(prepared.mime, mime);
            assert!(prepared.receipt.is_none());
        }
        let bytes = std::fs::read(format!("{FIXTURES}/gif-offset-malformed-later.gif")).unwrap();
        assert!(prepare_embedding_image(&hash_bytes(&bytes), bytes, "image/gif").is_err());
    }
}
