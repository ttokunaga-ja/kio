//! Embedding record provenance (03 §8.1); independent of semantic identity.
use super::{embedding_corrupt_error, is_hash};
use crate::error::Result;
use serde::{Deserialize, Deserializer, Serialize};

// `Option` fields are nullable, but absent keys are schema violations.
fn required_nullable<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProducerExecutionMode {
    OnlineApi,
    OfflineApi,
    NativeLibrary,
    DeterministicLibrary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProducerProvenance {
    pub execution_mode: ProducerExecutionMode,
    pub os: String,
    pub arch: String,
    #[serde(deserialize_with = "required_nullable")]
    pub runtime_version: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub runtime_library_sha256: Option<String>,
    pub profile_hash: String,
    #[serde(deserialize_with = "required_nullable")]
    pub cpu_backend: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub threads: Option<u64>,
}

impl ProducerProvenance {
    /// Record the actual executing non-native adapter and device, without
    /// inventing native runtime information.
    pub fn non_native(mode: ProducerExecutionMode, profile_hash: &str) -> Result<Self> {
        let producer = Self {
            execution_mode: mode,
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            runtime_version: None,
            runtime_library_sha256: None,
            profile_hash: profile_hash.to_owned(),
            cpu_backend: None,
            threads: None,
        };
        producer.validate(profile_hash)?;
        Ok(producer)
    }

    fn validate(&self, profile_hash: &str) -> Result<()> {
        if !is_hash(&self.profile_hash) || self.profile_hash != profile_hash {
            return Err(embedding_corrupt_error(
                "producer profile hash is invalid or unbound",
                None,
            ));
        }
        if self.os.trim().is_empty() || self.arch.trim().is_empty() {
            return Err(embedding_corrupt_error(
                "producer device identifiers must not be empty",
                None,
            ));
        }
        // A native record needs authenticated profile/artifact-set context,
        // which the current non-native CAS boundary does not have. Syntactic
        // provenance alone must never authenticate a runtime library.
        if self.execution_mode == ProducerExecutionMode::NativeLibrary {
            return Err(embedding_corrupt_error(
                "native producer requires authenticated artifact-set validation",
                None,
            ));
        }
        if self.runtime_version.is_some()
            || self.runtime_library_sha256.is_some()
            || self.cpu_backend.is_some()
            || self.threads.is_some()
        {
            return Err(embedding_corrupt_error(
                "non-native producer must have null native runtime fields",
                None,
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageInputReceipt {
    pub source_hash: String,
    pub source_mime: String,
    pub sent_hash: String,
    pub sent_mime: String,
    pub transcoder_pin: String,
}

impl ImageInputReceipt {
    pub fn validate(&self) -> Result<()> {
        if !is_hash(&self.source_hash)
            || !is_hash(&self.sent_hash)
            || !matches!(self.source_mime.as_str(), "image/gif" | "image/webp")
            || self.sent_mime != "image/png"
            || self.transcoder_pin.trim().is_empty()
        {
            return Err(embedding_corrupt_error(
                "image input receipt has invalid hashes, MIME or pin",
                None,
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingMetadata {
    pub record_format: u64,
    pub producer: ProducerProvenance,
    #[serde(deserialize_with = "required_nullable")]
    pub image_input_receipt: Option<ImageInputReceipt>,
}

impl EmbeddingMetadata {
    pub(super) fn validate(&self, profile_hash: &str) -> Result<()> {
        if self.record_format != 2 {
            return Err(embedding_corrupt_error(
                "embedding record_format must be exactly 2",
                None,
            ));
        }
        self.producer.validate(profile_hash)?;
        if self.image_input_receipt.is_some() {
            return Err(embedding_corrupt_error(
                "image receipt requires authenticated typed input context",
                None,
            ));
        }
        Ok(())
    }
}
