use serde::{Deserialize, Serialize};

/// Required provenance for both initial and subsequent immutable manifests.
/// Unit entries record preparation hashes; parent_gen retains the old instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "UncheckedPreparationProvenance")]
pub struct PreparationProvenance {
    pub renderer_name: Option<String>,
    pub renderer_version: Option<String>,
    pub producer_os: String,
    pub producer_arch: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UncheckedPreparationProvenance {
    #[serde(deserialize_with = "deserialize_required_nullable_string")]
    renderer_name: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable_string")]
    renderer_version: Option<String>,
    producer_os: String,
    producer_arch: String,
}

impl TryFrom<UncheckedPreparationProvenance> for PreparationProvenance {
    type Error = &'static str;

    fn try_from(value: UncheckedPreparationProvenance) -> Result<Self, Self::Error> {
        let provenance = Self {
            renderer_name: value.renderer_name,
            renderer_version: value.renderer_version,
            producer_os: value.producer_os,
            producer_arch: value.producer_arch,
        };
        if !provenance.is_valid() {
            return Err("invalid preparation producer or renderer identity");
        }
        Ok(provenance)
    }
}

impl PreparationProvenance {
    #[must_use]
    pub fn is_valid(&self) -> bool {
        !self.producer_os.trim().is_empty()
            && !self.producer_arch.trim().is_empty()
            && match (&self.renderer_name, &self.renderer_version) {
                (None, None) => true,
                (Some(name), Some(version)) => {
                    !name.trim().is_empty() && !version.trim().is_empty()
                }
                _ => false,
            }
    }

    #[must_use]
    pub fn local() -> Self {
        let (os, arch) = crate::platform::producer_platform();
        Self {
            renderer_name: None,
            renderer_version: None,
            producer_os: os.to_owned(),
            producer_arch: arch.to_owned(),
        }
    }
}

fn deserialize_required_nullable_string<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)
}
