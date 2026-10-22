//! Deterministic ingestion of standalone raster images.
//!
//! Image bytes remain binary CAS content.  The normalized unit only carries a
//! stable `kio://` image-object reference, which lets the normal chunk and image
//! vector pipelines discover pixels without treating binary data as text.

mod budget_gif;
mod budget_jpeg;
mod budget_jpeg_metadata;
mod budget_png;
mod budget_vp8;
mod budget_webp;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::json;

use kio_adapter::types::MAX_EMBEDDING_ITEM_BYTES;
use kio_core::cas::{ContentObjectKind, is_hash};
use kio_core::dag::NormalizeRef;
use kio_core::scope::{Repository, new_ulid};
use kio_core::store_dir::StoreDirectory;
use kio_core::{KioError, Result};
use kio_pipeline::markdownize::{
    MarkdownizeMode, NormalizedInstanceManifest, NormalizedUnitManifestEntry, NormalizedUnitObject,
    UnitStatus, persist_normalized_instance_bound,
};
use kio_pipeline::prepare::{UnitType, hash_bytes, unit_ref};
use kio_search::extract_related_images;

pub(crate) struct ImageIngested {
    pub(crate) normalize: NormalizeRef,
}

/// The retention boundary distinguishes malformed/provider-inconsistent source
/// data from local CAS publication failures. Callers that already hold a paid
/// provider result must retain the latter for recovery rather than report a
/// provider contract violation and submit it again.
#[derive(Debug)]
pub(crate) enum SourceImageRetentionError {
    SourceValidation(KioError),
    LocalPersistence(KioError),
}

/// Preserve the verified source raster for an OCR-discovered image unit.
///
/// OCR providers commonly return only extracted text for a standalone image.
/// The text remains the provider's output, but it must also cite the original
/// source image object: the ordinary image-vector and replica projections are
/// intentionally derived from normalized Markdown, not from transient adapter
/// response fields.  This is the shared normalization boundary for every OCR
/// adapter, rather than a provider-specific response rewrite.
///
/// `bytes` is the caller's already-bound raw buffer.  We never reopen a path
/// here; the CAS identity and the declared media type are verified before an
/// image URI can be emitted.
pub(crate) fn retain_original_source_image_reference(
    repo: &Repository,
    raw_hash: &str,
    bytes: &[u8],
    declared_media_type: &str,
    unit_type: UnitType,
    markdown: &str,
) -> std::result::Result<String, SourceImageRetentionError> {
    if unit_type != UnitType::Image {
        return Ok(markdown.to_owned());
    }
    let validated = validate_source_image_payload(bytes, declared_media_type)
        .map_err(SourceImageRetentionError::SourceValidation)?;
    validate_source_image_identity(raw_hash, &validated)
        .map_err(SourceImageRetentionError::SourceValidation)?;
    let image_hash = persist_validated_source_image(repo, raw_hash, &validated)
        .map_err(SourceImageRetentionError::LocalPersistence)?;
    let scope_id = repo
        .scope_identity()
        .map_err(SourceImageRetentionError::LocalPersistence)?
        .scope_id;
    let image_uri = format!("kio://{scope_id}/object/image/{image_hash}");
    if extract_related_images(markdown)
        .iter()
        .any(|reference| reference.image_uri == image_uri)
    {
        return Ok(markdown.to_owned());
    }

    let mut retained = markdown.to_owned();
    if !retained.is_empty() && !retained.ends_with('\n') {
        retained.push('\n');
    }
    if !retained.ends_with("\n\n") {
        retained.push('\n');
    }
    retained.push_str("![source image](");
    retained.push_str(&image_uri);
    retained.push_str(")\n");
    Ok(retained)
}

fn validate_source_image_identity(
    raw_hash: &str,
    validated: &ValidatedSourceImage<'_>,
) -> Result<()> {
    if hash_bytes(validated.bytes) != raw_hash || !is_hash(raw_hash) {
        return Err(KioError::schema(
            "source image bytes do not match the verified raw identity",
        ));
    }
    if validated.bytes.len() > MAX_EMBEDDING_ITEM_BYTES {
        return Err(KioError::schema(format!(
            "source image exceeds the embedding input byte limit of {MAX_EMBEDDING_ITEM_BYTES}",
        )));
    }
    Ok(())
}

/// Write and immediately re-verify the fully decoded source image object used
/// by the canonical Markdown reference. This shares the bounded, immutable
/// content object path with standalone image ingestion while retaining OCR
/// text as the normalized unit's primary content.
fn persist_validated_source_image(
    repo: &Repository,
    raw_hash: &str,
    validated: &ValidatedSourceImage<'_>,
) -> Result<String> {
    let bytes = validated.bytes;
    let image_hash = repo
        .object_store()
        .write_content_object(ContentObjectKind::Image, bytes)?;
    if image_hash != raw_hash {
        return Err(KioError::schema(
            "source image CAS identity differs from the verified raw identity",
        ));
    }
    repo.object_store()
        .inspect_content_object(ContentObjectKind::Image, &image_hash)?;
    Ok(image_hash)
}

const MAX_IMAGE_WIDTH: u32 = 8192;
const MAX_IMAGE_HEIGHT: u32 = 8192;
const MAX_IMAGE_DECODED_BYTES: u64 = 64 * 1024 * 1024;
const MAX_IMAGE_PIXELS: u64 = 16_000_000;
const MAX_GIF_FRAMES: u64 = 32;

/// Opaque outside this module: CAS publication receives this token only after
/// the full decoder has accepted the exact byte slice. Other admission gates
/// may call [`validate_source_image_payload`] and discard the token to obtain
/// the same bounded decode without writing a CAS object.
pub(crate) struct ValidatedSourceImage<'a> {
    bytes: &'a [u8],
    kind: ValidatedSourceImageKind,
}

enum ValidatedSourceImageKind {
    Native {
        mime: String,
        width: u32,
        height: u32,
    },
    Gif,
}

/// Decode source pixels before making them reachable from normalized Markdown.
/// PNG/JPEG/WebP use the same bounded decoder as native standalone ingestion.
/// GIF remains OCR-supported even though it is not a native standalone format,
/// so its animation is consumed frame-by-frame under equivalent per-frame and
/// aggregate limits before the source object becomes embeddable.
pub(crate) fn validate_source_image_payload<'a>(
    bytes: &'a [u8],
    declared_media_type: &str,
) -> Result<ValidatedSourceImage<'a>> {
    if bytes.len() > MAX_EMBEDDING_ITEM_BYTES {
        return Err(KioError::schema(format!(
            "source image exceeds the embedding input byte limit of {MAX_EMBEDDING_ITEM_BYTES}",
        )));
    }
    match declared_media_type {
        "image/png" | "image/jpeg" | "image/webp" => {
            let (mime, width, height) = inspect_image(bytes, declared_media_type)?;
            Ok(ValidatedSourceImage {
                bytes,
                kind: ValidatedSourceImageKind::Native {
                    mime,
                    width,
                    height,
                },
            })
        }
        "image/gif" => {
            inspect_gif(bytes)?;
            Ok(ValidatedSourceImage {
                bytes,
                kind: ValidatedSourceImageKind::Gif,
            })
        }
        _ => Err(KioError::schema("source image media type is unsupported")),
    }
}

fn inspect_gif(bytes: &[u8]) -> Result<()> {
    budget_gif::validate_gif_layout(bytes)?;
    use std::io::{BufReader, Cursor};

    use image::codecs::gif::GifDecoder;
    use image::{AnimationDecoder, ImageDecoder};

    let mut decoder = GifDecoder::new(BufReader::new(Cursor::new(bytes)))
        .map_err(|error| KioError::schema(format!("GIF decode validation failed: {error}")))?;
    decoder
        .set_limits(image_decode_limits())
        .map_err(|error| KioError::schema(format!("GIF decode limit rejected input: {error}")))?;
    let (width, height) = decoder.dimensions();
    validate_decoded_dimensions(width, height)?;

    let mut frame_count = 0_u64;
    let mut total_pixels = 0_u64;
    let mut total_bytes = 0_u64;
    for frame in decoder.into_frames() {
        let frame = frame.map_err(|error| {
            KioError::schema(format!("GIF frame decode validation failed: {error}"))
        })?;
        frame_count = frame_count
            .checked_add(1)
            .ok_or_else(|| KioError::schema("GIF frame count overflows"))?;
        if frame_count > MAX_GIF_FRAMES {
            return Err(KioError::schema("GIF exceeds the supported frame limit"));
        }
        let (frame_width, frame_height) = frame.buffer().dimensions();
        let pixels = checked_image_pixels(frame_width, frame_height)?;
        let frame_bytes = pixels
            .checked_mul(4)
            .ok_or_else(|| KioError::schema("GIF decoded frame byte size overflows"))?;
        total_pixels = total_pixels
            .checked_add(pixels)
            .ok_or_else(|| KioError::schema("GIF total decoded pixels overflow"))?;
        total_bytes = total_bytes
            .checked_add(frame_bytes)
            .ok_or_else(|| KioError::schema("GIF total decoded byte size overflows"))?;
        if total_pixels > MAX_IMAGE_PIXELS || total_bytes > MAX_IMAGE_DECODED_BYTES {
            return Err(KioError::schema(
                "GIF exceeds the supported total decoded pixel or byte limit",
            ));
        }
        // `frame` drops here; never retain decoded animation frames.
    }
    if frame_count == 0 {
        return Err(KioError::schema("GIF contains no decodable frames"));
    }
    Ok(())
}

/// Persist one standalone image as an immutable image CAS object and a normal
/// single-image normalized instance. The caller has already bound the bytes to
/// the scanned regular file; this function never reopens that path.
pub(crate) fn ingest_standalone_image(
    repo: &Repository,
    input_path: &str,
    raw_hash: &str,
    bytes: &[u8],
    declared_media_type: &str,
    _preparation_profile_hash: &str,
    _markdown_profile_hash: &str,
) -> Result<ImageIngested> {
    if hash_bytes(bytes) != raw_hash || !is_hash(raw_hash) {
        return Err(KioError::schema(
            "standalone image bytes do not match the verified raw identity",
        ));
    }
    if bytes.len() > MAX_EMBEDDING_ITEM_BYTES {
        return Err(KioError::schema(format!(
            "standalone image exceeds the embedding input byte limit of {MAX_EMBEDDING_ITEM_BYTES}",
        )));
    }
    let validated = validate_source_image_payload(bytes, declared_media_type)?;
    let ValidatedSourceImage {
        kind:
            ValidatedSourceImageKind::Native {
                mime,
                width,
                height,
            },
        ..
    } = &validated
    else {
        return Err(KioError::schema(
            "standalone image media type is unsupported",
        ));
    };
    let native_profile_hash = native_image_profile_hash();

    validate_source_image_identity(raw_hash, &validated)?;
    let image_hash = persist_validated_source_image(repo, raw_hash, &validated)?;
    let prepared_hash = repo
        .object_store()
        .write_content_object(ContentObjectKind::Prepared, bytes)?;
    if prepared_hash != raw_hash {
        return Err(KioError::schema(
            "standalone image prepared CAS identity differs from the verified raw identity",
        ));
    }

    let scope_id = repo.scope_identity()?.scope_id;
    let unit_key = "image:0".to_owned();
    let generated_at = repo.control().now_utc_seconds();
    let alt = safe_alt_label(input_path);
    let image_uri = format!("kio://{scope_id}/object/image/{image_hash}");
    let unit = NormalizedUnitObject {
        unit_key: unit_key.clone(),
        unit_type: UnitType::Image,
        raw_hash: raw_hash.to_owned(),
        // The image object is the prepared source; it is never represented as
        // text or copied to objects/prepared.
        prepared_hash: prepared_hash.clone(),
        preparation_profile_hash: native_profile_hash.clone(),
        tool_profile_hash: native_profile_hash.clone(),
        r#gen: 0,
        mode: MarkdownizeMode::Full,
        markdown: format!("![{alt}]({image_uri})\n"),
        // This direct ingestion path created and verified the image CAS bytes
        // above, so it is the typed owner of exactly that immutable object.
        owned_image_hashes: BTreeSet::from([image_hash.clone()]),
        metadata: BTreeMap::from([
            ("image_object_hash".to_owned(), json!(image_hash)),
            ("image_uri".to_owned(), json!(image_uri)),
            ("mime".to_owned(), json!(mime)),
            ("width".to_owned(), json!(width)),
            ("height".to_owned(), json!(height)),
            ("alt".to_owned(), json!(alt)),
        ]),
        reused_from: None,
        generated_at: generated_at.clone(),
    };
    let manifest = NormalizedInstanceManifest {
        raw_hash: raw_hash.to_owned(),
        tool_profile_hash: native_profile_hash.clone(),
        r#gen: 0,
        parent_gen: None,
        run_id: format!("run_{}", new_ulid(repo.canonical_root())),
        units: vec![NormalizedUnitManifestEntry {
            order: 0,
            unit_key: unit_key.clone(),
            unit_ref: unit_ref(&unit_key),
            unit_type: UnitType::Image,
            status: UnitStatus::Done,
            prepared_hash,
            preparation_profile_hash: native_profile_hash.clone(),
            unit_object_hash: None,
            error_kind: None,
        }],
        generated_at,
    };
    let retained_kio = repo
        .bound_kio_handle()
        .ok_or_else(|| {
            KioError::invalid_usage("native image ingestion requires retained .kio authority")
        })?
        .try_clone()
        .map_err(|error| KioError::io(error.to_string(), ".kio"))?;
    let directory = StoreDirectory::from_retained_with_control(
        retained_kio,
        repo.kio_dir().to_path_buf(),
        repo.control().clone(),
    )?;
    let stamped = persist_normalized_instance_bound(&directory, &manifest, &[unit])
        .map_err(|error| KioError::schema(error.to_string()))?;
    let manifest_hash = crate::object_persistence::hash_and_write_manifest_object(repo, &stamped)?;
    Ok(ImageIngested {
        normalize: NormalizeRef {
            tool_profile_hash: native_profile_hash,
            r#gen: 0,
            manifest_hash,
        },
    })
}

fn inspect_image(bytes: &[u8], declared_media_type: &str) -> Result<(String, u32, u32)> {
    use image::{DynamicImage, ImageDecoder, ImageFormat};
    use std::io::Cursor;
    let expected = match declared_media_type {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        "image/webp" => ImageFormat::WebP,
        _ => {
            return Err(KioError::schema(
                "standalone image media type is unsupported",
            ));
        }
    };
    if image::guess_format(bytes).ok() != Some(expected) {
        return Err(KioError::schema(
            "standalone image media type disagrees with verified image bytes",
        ));
    }
    if expected == ImageFormat::WebP {
        let (_, width, height, _) = decode_webp_rgba(bytes)?;
        return Ok((declared_media_type.to_owned(), width, height));
    }
    let decoded = if expected == ImageFormat::Png {
        // PNG's internal counter cannot be changed by set_limits after reader
        // construction. Reserve the exact EXPAND output before construction.
        let decoder = image::codecs::png::PngDecoder::with_limits(
            Cursor::new(bytes),
            budget_png::png_limits(bytes)?,
        )
        .map_err(decode_error)?;
        DynamicImage::from_decoder(decoder)
    } else {
        budget_jpeg::validate_jpeg_allocation(bytes)?;
        let mut decoder =
            image::codecs::jpeg::JpegDecoder::new(Cursor::new(bytes)).map_err(decode_error)?;
        decoder
            .set_limits(image_decode_limits())
            .map_err(decode_error)?;
        DynamicImage::from_decoder(decoder)
    }
    .map_err(decode_error)?;
    let (width, height) = (decoded.width(), decoded.height());
    validate_decoded_dimensions(width, height)?;
    Ok((declared_media_type.to_owned(), width, height))
}

fn decode_error(error: image::ImageError) -> KioError {
    KioError::schema(format!("image decode validation failed: {error}"))
}

/// Return frame zero on a transparent full canvas. GIF's entire animation is
/// validated first, without retaining any prior raster while the iterator runs.
pub(crate) fn decode_embedding_rgba(bytes: &[u8], mime: &str) -> Result<(Vec<u8>, u32, u32, u8)> {
    use image::{ImageDecoder, ImageFormat};
    use std::io::Cursor;
    match mime {
        "image/gif" => {
            validate_source_image_payload(bytes, mime)?;
            let mut decoder =
                image::codecs::gif::GifDecoder::new(Cursor::new(bytes)).map_err(decode_error)?;
            decoder
                .set_limits(image_decode_limits())
                .map_err(decode_error)?;
            let (width, height) = decoder.dimensions();
            let mut rgba = vec![0; decoder.total_bytes() as usize];
            // Account for the caller output in the decoder's optional offset
            // subframe allocation allowance (ImageReader normally does this).
            let mut limits = image_decode_limits();
            limits.max_alloc = Some(MAX_IMAGE_DECODED_BYTES - rgba.len() as u64);
            decoder.set_limits(limits).map_err(decode_error)?;
            decoder.read_image(&mut rgba).map_err(decode_error)?;
            Ok((rgba, width, height, 1))
        }
        "image/webp" => {
            if bytes.len() > MAX_EMBEDDING_ITEM_BYTES
                || image::guess_format(bytes).ok() != Some(ImageFormat::WebP)
            {
                return Err(KioError::schema(
                    "source WebP exceeds source limit or disagrees with declared MIME",
                ));
            }
            decode_webp_rgba(bytes)
        }
        _ => Err(KioError::schema(
            "image conversion media type is unsupported",
        )),
    }
}

fn decode_webp_rgba(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32, u8)> {
    use image::ImageDecoder;
    use std::io::Cursor;
    let mut decoder =
        image::codecs::webp::WebPDecoder::new(Cursor::new(bytes)).map_err(decode_error)?;
    let (width, height) = decoder.dimensions();
    validate_decoded_dimensions(width, height)?;
    let rgba_bytes = checked_image_pixels(width, height)? * 4;
    budget_webp::validate_webp_allocation(bytes, rgba_bytes)?;
    if decoder.has_animation() {
        decoder
            .set_background_color(image::Rgba([0, 0, 0, 0]))
            .map_err(decode_error)?;
    }
    // Metadata is read and dropped before allocating the decoded raster.
    let orientation = decoder.orientation().map_err(decode_error)?.to_exif();
    let decoded_bytes = decoder.total_bytes() as usize;
    let rgb = decoder.color_type() == image::ColorType::Rgb8;
    let mut rgba = vec![0; rgba_bytes as usize];
    decoder
        .read_image(&mut rgba[..decoded_bytes])
        .map_err(decode_error)?;
    if rgb {
        for i in (0..rgba.len() / 4).rev() {
            let pixel = [rgba[3 * i], rgba[3 * i + 1], rgba[3 * i + 2], 255];
            rgba[4 * i..4 * i + 4].copy_from_slice(&pixel);
        }
    }
    Ok((rgba, width, height, orientation))
}

fn image_decode_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_WIDTH);
    limits.max_image_height = Some(MAX_IMAGE_HEIGHT);
    limits.max_alloc = Some(MAX_IMAGE_DECODED_BYTES);
    limits
}

fn validate_decoded_dimensions(width: u32, height: u32) -> Result<()> {
    let pixels = checked_image_pixels(width, height)?;
    let decoded_bytes = pixels
        .checked_mul(4)
        .ok_or_else(|| KioError::schema("decoded image byte size overflows"))?;
    if decoded_bytes > MAX_IMAGE_DECODED_BYTES || pixels > MAX_IMAGE_PIXELS {
        return Err(KioError::schema(
            "decoded image exceeds the supported limit",
        ));
    }
    Ok(())
}

fn checked_image_pixels(width: u32, height: u32) -> Result<u64> {
    if width > MAX_IMAGE_WIDTH || height > MAX_IMAGE_HEIGHT {
        return Err(KioError::schema(
            "decoded image dimensions exceed the supported limit",
        ));
    }
    u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| KioError::schema("decoded image pixel count overflows"))
}

pub(crate) fn native_image_profile_hash() -> String {
    hash_bytes(b"kio-native-image-ingest/v1;png,jpeg,webp;decode-limits-8192-16m")
}

fn safe_alt_label(input_path: &str) -> String {
    let source = Path::new(input_path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("image");
    let mut label = String::new();
    for ch in source.chars().take(160) {
        match ch {
            '\\' | '[' | ']' | '(' | ')' => {
                label.push('\\');
                label.push(ch);
            }
            ch if ch.is_control() => label.push(' '),
            ch => label.push(ch),
        }
    }
    if label.is_empty() {
        "image".to_owned()
    } else {
        label
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shared acceptance fixture, independently decoder-valid (278x278 PNG).
    const PNG: &[u8] = include_bytes!(
        "../../kio-eval/acceptance-fixtures/v1/authenticated-local-public/image.png"
    );
    // A complete, static 1x1 GIF89a stream, not merely a header-shaped probe.
    const STATIC_GIF: &[u8] = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff\x21\xf9\x04\x01\x00\x00\x00\x00\x2c\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02\x44\x01\x00\x3b";

    #[test]
    fn text_only_ocr_image_keeps_verified_source_reference_once() {
        let root = tempfile::tempdir().unwrap();
        let repo = Repository::init(root.path()).unwrap();
        let raw_hash = hash_bytes(PNG);

        let retained = retain_original_source_image_reference(
            &repo,
            &raw_hash,
            PNG,
            "image/png",
            UnitType::Image,
            "recognized OCR text\n",
        )
        .unwrap();
        let scope_id = repo.scope_identity().unwrap().scope_id;
        let uri = format!("kio://{scope_id}/object/image/{raw_hash}");
        assert_eq!(extract_related_images(&retained).len(), 1);
        assert!(retained.contains(&uri));
        repo.object_store()
            .inspect_content_object(ContentObjectKind::Image, &raw_hash)
            .unwrap();

        let repeated = retain_original_source_image_reference(
            &repo,
            &raw_hash,
            PNG,
            "image/png",
            UnitType::Image,
            &retained,
        )
        .unwrap();
        assert_eq!(repeated, retained);
    }

    #[test]
    fn non_image_unit_never_writes_or_appends_source_reference() {
        let root = tempfile::tempdir().unwrap();
        let repo = Repository::init(root.path()).unwrap();
        let raw_hash = hash_bytes(PNG);
        let markdown = "provider text\n";
        assert_eq!(
            retain_original_source_image_reference(
                &repo,
                &raw_hash,
                PNG,
                "image/png",
                UnitType::Page,
                markdown,
            )
            .unwrap(),
            markdown
        );
        assert!(
            repo.object_store()
                .inspect_content_object(ContentObjectKind::Image, &raw_hash)
                .is_err()
        );
    }

    #[test]
    fn source_reference_rejects_mismatched_raw_identity_before_cas_write() {
        let root = tempfile::tempdir().unwrap();
        let repo = Repository::init(root.path()).unwrap();
        let wrong_hash = hash_bytes(b"different bytes");

        assert!(
            retain_original_source_image_reference(
                &repo,
                &wrong_hash,
                PNG,
                "image/png",
                UnitType::Image,
                "recognized OCR text\n",
            )
            .is_err()
        );
        assert!(
            repo.object_store()
                .inspect_content_object(ContentObjectKind::Image, &wrong_hash)
                .is_err()
        );
    }

    #[test]
    fn source_reference_rejects_declared_media_mismatch_before_cas_write() {
        let root = tempfile::tempdir().unwrap();
        let repo = Repository::init(root.path()).unwrap();
        let raw_hash = hash_bytes(PNG);

        assert!(
            retain_original_source_image_reference(
                &repo,
                &raw_hash,
                PNG,
                "image/jpeg",
                UnitType::Image,
                "recognized OCR text\n",
            )
            .is_err()
        );
        assert!(
            repo.object_store()
                .inspect_content_object(ContentObjectKind::Image, &raw_hash)
                .is_err()
        );
    }

    #[test]
    fn source_reference_classifies_image_cas_publication_failure_as_local_persistence() {
        let root = tempfile::tempdir().unwrap();
        let repo = Repository::init(root.path()).unwrap();
        // The retained object-store handle resolves `objects/image` lazily. A
        // regular file at that name deterministically makes the local CAS
        // publication fail after decode and source-identity validation.
        std::fs::write(repo.kio_dir().join("objects/image"), b"not a directory").unwrap();
        let raw_hash = hash_bytes(PNG);

        assert!(matches!(
            retain_original_source_image_reference(
                &repo,
                &raw_hash,
                PNG,
                "image/png",
                UnitType::Image,
                "recognized OCR text\n",
            ),
            Err(SourceImageRetentionError::LocalPersistence(_))
        ));
    }

    #[test]
    fn valid_static_gif_is_fully_decoded_before_source_reference_is_persisted() {
        let root = tempfile::tempdir().unwrap();
        let repo = Repository::init(root.path()).unwrap();
        let raw_hash = hash_bytes(STATIC_GIF);

        let markdown = retain_original_source_image_reference(
            &repo,
            &raw_hash,
            STATIC_GIF,
            "image/gif",
            UnitType::Image,
            "recognized OCR text\n",
        )
        .unwrap();
        assert_eq!(extract_related_images(&markdown).len(), 1);
        repo.object_store()
            .inspect_content_object(ContentObjectKind::Image, &raw_hash)
            .unwrap();
    }

    #[test]
    fn oversized_gif_screen_is_rejected_before_cas_write() {
        // Logical screen 8193x1. The decoder's dimensions gate rejects this
        // before it reaches a frame allocation or Image CAS publication.
        let mut oversized = STATIC_GIF.to_vec();
        oversized[6] = 1;
        oversized[7] = 0x20;
        let root = tempfile::tempdir().unwrap();
        let repo = Repository::init(root.path()).unwrap();
        let raw_hash = hash_bytes(&oversized);

        let error = retain_original_source_image_reference(
            &repo,
            &raw_hash,
            &oversized,
            "image/gif",
            UnitType::Image,
            "recognized OCR text\n",
        )
        .unwrap_err();
        assert!(matches!(
            error,
            SourceImageRetentionError::SourceValidation(error) if error.message().contains("limit")
        ));
        assert!(
            repo.object_store()
                .inspect_content_object(ContentObjectKind::Image, &raw_hash)
                .is_err()
        );
    }

    #[test]
    fn animated_gif_over_frame_limit_is_rejected_before_cas_write() {
        let frames = (0..=MAX_GIF_FRAMES)
            .map(|_| {
                image::Frame::new(image::RgbaImage::from_pixel(
                    1,
                    1,
                    image::Rgba([0, 0, 0, 255]),
                ))
            })
            .collect::<Vec<_>>();
        let mut animated = Vec::new();
        image::codecs::gif::GifEncoder::new(&mut animated)
            .encode_frames(frames)
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        let repo = Repository::init(root.path()).unwrap();
        let raw_hash = hash_bytes(&animated);

        assert!(
            retain_original_source_image_reference(
                &repo,
                &raw_hash,
                &animated,
                "image/gif",
                UnitType::Image,
                "recognized OCR text\n",
            )
            .is_err()
        );
        assert!(
            repo.object_store()
                .inspect_content_object(ContentObjectKind::Image, &raw_hash)
                .is_err()
        );
    }

    #[test]
    fn animated_gif_over_total_decode_budget_is_rejected_before_cas_write() {
        // Five complete 2048x2048 RGBA frames are fewer than the frame cap but
        // exceed both aggregate limits. Encode incrementally so the fixture
        // itself does not retain every uncompressed frame at once.
        let mut animated = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut animated);
            for _ in 0..5 {
                encoder
                    .encode_frame(image::Frame::new(image::RgbaImage::from_pixel(
                        2048,
                        2048,
                        image::Rgba([0, 0, 0, 255]),
                    )))
                    .unwrap();
            }
        }
        let root = tempfile::tempdir().unwrap();
        let repo = Repository::init(root.path()).unwrap();
        let raw_hash = hash_bytes(&animated);

        let error = retain_original_source_image_reference(
            &repo,
            &raw_hash,
            &animated,
            "image/gif",
            UnitType::Image,
            "recognized OCR text\n",
        )
        .unwrap_err();
        assert!(matches!(
            error,
            SourceImageRetentionError::SourceValidation(error)
                if error.message().contains("total decoded")
        ));
        assert!(
            repo.object_store()
                .inspect_content_object(ContentObjectKind::Image, &raw_hash)
                .is_err()
        );
    }

    /// 07 §5.3 / 14: conversion of frame zero cannot bypass validation of
    /// later GIF frames at the existing OCR original-source retention gate.
    #[test]
    fn malformed_later_gif_frame_rejects_source_retention_after_valid_first_frame() {
        use image::AnimationDecoder;
        use image::codecs::gif::GifDecoder;
        use std::io::{BufReader, Cursor};

        const VALID: &[u8] =
            include_bytes!("../../kio-cli/tests/fixtures/o2_golden/gif-offset-poison-later.gif");
        const MALFORMED: &[u8] =
            include_bytes!("../../kio-cli/tests/fixtures/o2_golden/gif-offset-malformed-later.gif");
        let valid_hash = hash_bytes(VALID);
        let malformed_hash = hash_bytes(MALFORMED);
        assert_eq!(
            valid_hash,
            "sha256:b852a8df6594cd3077ac6a255038a57619e89ab50a1c181af5782b119ff9574c"
        );
        assert_eq!(
            malformed_hash,
            "sha256:4ee1e35c9e07cfa804810fda06d97dbc4e4edab2a60a15a3f2acc0ede1486e22"
        );
        // Prove the fixture reaches a valid first full-canvas frame. The
        // source differs in the later frame; a frame-zero-only path is unsafe.
        let mut valid_frames = GifDecoder::new(BufReader::new(Cursor::new(VALID)))
            .unwrap()
            .into_frames();
        let valid_first = valid_frames.next().unwrap().unwrap();
        let mut malformed_frames = GifDecoder::new(BufReader::new(Cursor::new(MALFORMED)))
            .unwrap()
            .into_frames();
        let malformed_first = malformed_frames.next().unwrap().unwrap();
        assert_eq!(valid_first.buffer().dimensions(), (4, 3));
        assert_eq!(malformed_first.buffer(), valid_first.buffer());
        assert!(
            malformed_frames.next().unwrap().is_err(),
            "the unchanged first frame must not mask the malformed later LZW stream"
        );
        drop(valid_frames);
        drop(malformed_frames);

        let root = crate::test_support::PrivateTempDir::new();
        let repo = Repository::init(root.path()).unwrap();
        let original_head = repo.head_commit_hash().unwrap();
        const OCR_TEXT: &str = "recognized OCR text\n";
        let retained = retain_original_source_image_reference(
            &repo,
            &valid_hash,
            VALID,
            "image/gif",
            UnitType::Image,
            OCR_TEXT,
        )
        .expect("the same first-frame control with valid later data must retain its source");
        let references = extract_related_images(&retained);
        assert_eq!(references.len(), 1);
        assert_eq!(
            references[0].image_uri,
            format!(
                "kio://{}/object/image/{valid_hash}",
                repo.scope_identity().unwrap().scope_id
            )
        );
        assert_eq!(
            repo.object_store()
                .read_content_object_bytes(
                    ContentObjectKind::Image,
                    &valid_hash,
                    VALID.len() as u64,
                )
                .unwrap(),
            VALID
        );

        let failure = retain_original_source_image_reference(
            &repo,
            &malformed_hash,
            MALFORMED,
            "image/gif",
            UnitType::Image,
            OCR_TEXT,
        )
        .expect_err("malformed later frame must return no source-reference Markdown");
        assert!(
            matches!(failure, SourceImageRetentionError::SourceValidation(error)
            if error.error_code() == "KIO-E-CONFIG-SCHEMA-001")
        );
        let missing = repo
            .object_store()
            .inspect_content_object(ContentObjectKind::Image, &malformed_hash)
            .expect_err("failed animation validation must publish no original Image CAS");
        assert_eq!(missing.error_code(), "KIO-E-STORE-NOT-FOUND-001");
        // This boundary returns source-reference Markdown only on success. It
        // neither publishes a normalized owner nor changes a source snapshot
        // when validation fails; existing valid source authority stays exact.
        assert_eq!(repo.head_commit_hash().unwrap(), original_head);
        assert_eq!(
            repo.object_store()
                .read_content_object_bytes(
                    ContentObjectKind::Image,
                    &valid_hash,
                    VALID.len() as u64,
                )
                .unwrap(),
            VALID
        );
    }
}
