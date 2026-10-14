use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct SkippedUnit {
    pub raw_hash: String,
    pub path: String,
    pub r#gen: u64,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PublishedSkippedUnit {
    #[serde(flatten)]
    pub unit: SkippedUnit,
    pub searchable: bool,
    pub guidance: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkippedDisclosure {
    pub skipped_units: Vec<PublishedSkippedUnit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped_units_guidance: Option<String>,
}
