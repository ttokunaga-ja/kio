use kio_core::cas::{ImageInputReceipt, is_hash};
use serde::{Deserialize, Serialize};

/// Frozen member identity. No source body or query text belongs in this value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EmbeddingTaskTarget {
    Chunk {
        chunk_hash: String,
        embedding_hash: String,
        profile_hash: String,
        input_hash: String,
    },
    Image {
        image_hash: String,
        embedding_hash: String,
        profile_hash: String,
        input_hash: String,
        #[serde(deserialize_with = "required_receipt")]
        image_input_receipt: Option<ImageInputReceipt>,
    },
    Query {
        query_hash: String,
        profile_hash: String,
        input_hash: String,
    },
}

impl EmbeddingTaskTarget {
    pub fn output_ref(&self) -> String {
        match self {
            Self::Chunk {
                chunk_hash,
                embedding_hash,
                ..
            } => format!("embedding:{chunk_hash}/{embedding_hash}"),
            Self::Image {
                image_hash,
                embedding_hash,
                ..
            } => format!("embedding-image:{image_hash}/{embedding_hash}"),
            Self::Query {
                query_hash,
                profile_hash,
                ..
            } => format!("embedding-query:{query_hash}/{profile_hash}"),
        }
    }
    pub fn input_hash(&self) -> &str {
        match self {
            Self::Chunk { input_hash, .. }
            | Self::Image { input_hash, .. }
            | Self::Query { input_hash, .. } => input_hash,
        }
    }

    pub fn profile_hash(&self) -> &str {
        match self {
            Self::Chunk { profile_hash, .. }
            | Self::Image { profile_hash, .. }
            | Self::Query { profile_hash, .. } => profile_hash,
        }
    }

    pub fn is_valid(&self) -> bool {
        if !is_hash(self.input_hash()) || !is_hash(self.profile_hash()) {
            return false;
        }
        match self {
            Self::Chunk {
                chunk_hash,
                embedding_hash,
                ..
            } => is_hash(chunk_hash) && is_hash(embedding_hash),
            Self::Image {
                image_hash,
                embedding_hash,
                image_input_receipt,
                input_hash,
                ..
            } => {
                is_hash(image_hash)
                    && is_hash(embedding_hash)
                    && match image_input_receipt {
                        Some(receipt) => {
                            receipt.source_hash == *image_hash
                                && receipt
                                    .image_input_hash()
                                    .is_ok_and(|hash| hash == *input_hash)
                        }
                        None => input_hash == image_hash,
                    }
            }
            Self::Query {
                query_hash,
                input_hash,
                ..
            } => is_hash(query_hash) && query_hash == input_hash,
        }
    }
}

fn required_receipt<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<ImageInputReceipt>, D::Error> {
    Option::<ImageInputReceipt>::deserialize(deserializer)
}
