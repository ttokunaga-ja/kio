use crate::embedding_owners::RetainedEmbeddingChunk;
use crate::image_authority;
use kio_core::KioError;
use kio_core::Result;
use kio_search::object_uri::extract_related_images;
use std::collections::BTreeSet;

/// Infer image MIME from magic bytes rather than an untrusted path or label.
pub(crate) fn verified_image_mime(bytes: &[u8]) -> Result<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Ok("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Ok("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Ok("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Ok("image/webp")
    } else {
        Err(KioError::schema(
            "image object has unsupported or unverifiable MIME",
        ))
    }
}

/// Every image object hash referenced by a live chunk body and owned by a
/// currently eligible retained unit. Markdown is only a citation relation;
/// immutable typed ownership is what permits a local CAS read or vector row.
pub(crate) fn referenced_authorized_image_hashes_for_chunks(
    chunks: &[impl AsRef<RetainedEmbeddingChunk>],
    scope_id: &str,
    authority: &image_authority::ImageAuthorityMap,
    secrets_approved: bool,
) -> BTreeSet<String> {
    let mut hashes = BTreeSet::new();
    for chunk in chunks
        .iter()
        .map(AsRef::as_ref)
        .filter(|chunk| secrets_approved || !chunk.requires_secret_approval)
    {
        for image in extract_related_images(&chunk.text) {
            if let Some(hash) =
                authorized_local_image_object_hash(scope_id, authority, &image.image_uri)
                && (secrets_approved || !authority.requires_secret_approval(&hash))
            {
                hashes.insert(hash);
            }
        }
    }
    hashes
}

/// Only a URI naming this scope may cause a local image CAS read, vector row,
/// or projection association. Foreign-scope Markdown links remain intact for
/// their explicit resolver, but never alias same-hash bytes in this scope.
pub(crate) fn local_image_object_hash(scope_id: &str, image_uri: &str) -> Option<String> {
    let object = kio_search::object_uri::parse_object_uri(image_uri).ok()?;
    (object.is_image() && object.scope_id() == scope_id).then(|| object.hash().to_owned())
}

/// A same-scope URI is necessary but insufficient for a local image-object
/// operation. Its hash must also occur in a retained unit's typed ownership
/// set and have a currently allowed owner alias.
pub(crate) fn authorized_local_image_object_hash(
    scope_id: &str,
    authority: &image_authority::ImageAuthorityMap,
    image_uri: &str,
) -> Option<String> {
    let hash = local_image_object_hash(scope_id, image_uri)?;
    authority.permits(&hash).then_some(hash)
}
