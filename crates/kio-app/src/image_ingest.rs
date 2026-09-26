//! Deterministic ingestion of standalone raster images.
//!
//! Image bytes remain binary CAS content.  The normalized unit only carries a
//! stable `kio://` image-object reference, which lets the normal chunk and image
//! vector pipelines discover pixels without treating binary data as text.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::json;

use kio_adapter::types::MAX_EMBEDDING_ITEM_BYTES;
use kio_core::cas::{ContentObjectKind, is_hash};
use kio_core::dag::NormalizeRef;
use kio_core::scope::{Repository, new_ulid, now_utc_seconds};
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
    let generated_at = now_utc_seconds();
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
    let directory = StoreDirectory::from_retained(retained_kio, repo.kio_dir().to_path_buf())?;
    let stamped = persist_normalized_instance_bound(&directory, &manifest, &[unit])
        .map_err(|error| KioError::schema(error.to_string()))?;
    let manifest_hash = super::hash_and_write_manifest_object(repo, &stamped)?;
    Ok(ImageIngested {
        normalize: NormalizeRef {
            tool_profile_hash: native_profile_hash,
            r#gen: 0,
            manifest_hash,
        },
    })
}

fn inspect_image(bytes: &[u8], declared_media_type: &str) -> Result<(String, u32, u32)> {
    use std::io::Cursor;

    use image::{ImageFormat, ImageReader};

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
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| KioError::schema(format!("image format detection failed: {error}")))?;
    if reader.format() != Some(expected) {
        return Err(KioError::schema(
            "standalone image media type disagrees with verified image bytes",
        ));
    }
    reader.limits(image_decode_limits());
    let decoded = reader
        .decode()
        .map_err(|error| KioError::schema(format!("image decode validation failed: {error}")))?;
    let (width, height) = (decoded.width(), decoded.height());
    validate_decoded_dimensions(width, height)?;
    // Drop the decoded raster before any CAS mutation. It serves only as a
    // complete-format validation gate; the persisted authority is raw bytes.
    drop(decoded);
    Ok((declared_media_type.to_owned(), width, height))
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
}
