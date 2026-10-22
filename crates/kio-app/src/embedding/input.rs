use crate::embedding_owners::RetainedEmbeddingChunk;
use crate::image_authority;
use kio_adapter::types::MAX_EMBEDDING_ITEM_BYTES;
use kio_core::KioError;
use kio_core::Result;
use kio_core::cas::{ContentObjectKind, ObjectStore, hash_bytes};
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

/// Authenticate and decode the original source image before an embedding
/// send or a CAS-backed image projection. The object read is bounded, and the
/// caller decides whether a missing source can be skipped or must fail.
pub(crate) fn read_verified_embedding_image(
    store: &ObjectStore,
    image_hash: &str,
) -> Result<(Vec<u8>, &'static str)> {
    store.inspect_content_object(ContentObjectKind::Image, image_hash)?;
    let bytes = store.read_content_object_bytes(
        ContentObjectKind::Image,
        image_hash,
        u64::try_from(MAX_EMBEDDING_ITEM_BYTES)
            .map_err(|_| KioError::schema("embedding image cap exceeds u64"))?,
    )?;
    if hash_bytes(&bytes) != image_hash {
        return Err(KioError::schema(
            "verified image payload hash changed while reading",
        ));
    }
    let mime = verified_image_mime(&bytes)?;
    let _ = crate::image_ingest::validate_source_image_payload(&bytes, mime)?;
    Ok((bytes, mime))
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

/// Frozen input identity; no corpus-sized image byte collection is retained.
#[derive(Clone)]
pub(crate) struct ImageEmbeddingPlan {
    pub(crate) target: kio_pipeline::task::EmbeddingTaskTarget,
    pub(crate) context: Option<kio_core::cas::EmbeddingContext>,
}

impl ImageEmbeddingPlan {
    pub(crate) fn context_key(&self) -> Option<&str> {
        match self.context.as_ref() {
            Some(kio_core::cas::EmbeddingContext::Image { image_input_hash }) => {
                Some(image_input_hash)
            }
            _ => None,
        }
    }
    pub(crate) fn receipt(&self) -> Option<&kio_core::cas::ImageInputReceipt> {
        match &self.target {
            kio_pipeline::task::EmbeddingTaskTarget::Image {
                image_input_receipt,
                ..
            } => image_input_receipt.as_ref(),
            _ => None,
        }
    }
    pub(crate) fn embedding_hash(&self) -> &str {
        match &self.target {
            kio_pipeline::task::EmbeddingTaskTarget::Image { embedding_hash, .. } => embedding_hash,
            _ => unreachable!("image input plan owns an image target"),
        }
    }
}

pub(crate) fn prepare_owned_embedding_image(
    store: &ObjectStore,
    image_hash: &str,
    profile: &kio_adapter::catalog::DeclaredEmbeddingProfile,
) -> Result<(
    ImageEmbeddingPlan,
    super::image_input::PreparedEmbeddingImage,
)> {
    let (bytes, mime) = read_verified_embedding_image(store, image_hash)?;
    let prepared = super::image_input::prepare_embedding_image(image_hash, bytes, mime)?;
    let input_hash = prepared
        .receipt
        .as_ref()
        .map(|receipt| receipt.image_input_hash())
        .transpose()?;
    let context = input_hash
        .as_ref()
        .map(|hash| kio_core::cas::EmbeddingContext::Image {
            image_input_hash: hash.clone(),
        });
    let embedding_hash = kio_index::embedding_store::embedding_hash(
        kio_index::EmbeddingTargetType::Image,
        image_hash,
        profile.dimensions,
        kio_index::EmbeddingDistance::Cosine,
        kio_index::EmbeddingModality::Multimodal,
        &profile.profile_hash,
        context.as_ref(),
    )
    .map_err(crate::object_persistence::index_to_kio)?;
    let target = kio_pipeline::task::EmbeddingTaskTarget::Image {
        image_hash: image_hash.to_owned(),
        embedding_hash,
        profile_hash: profile.profile_hash.clone(),
        input_hash: input_hash.unwrap_or_else(|| image_hash.to_owned()),
        image_input_receipt: prepared.receipt.clone(),
    };
    Ok((ImageEmbeddingPlan { target, context }, prepared))
}
