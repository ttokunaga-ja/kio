use serde_json::Value;

use crate::error::{KioError, Result};

#[derive(Debug, Clone, Copy)]
pub enum SchemaKind {
    Config,
    Scope,
    Manifest,
}

impl SchemaKind {
    const fn name(self) -> &'static str {
        match self {
            Self::Config => "config.toml",
            Self::Scope => "scope.json",
            Self::Manifest => "manifest.json",
        }
    }

    const fn schema_text(self) -> &'static str {
        match self {
            Self::Config => include_str!("../schemas/config.schema.json"),
            Self::Scope => include_str!("../schemas/scope.schema.json"),
            Self::Manifest => include_str!("../schemas/manifest.schema.json"),
        }
    }
}

pub fn validate_json_schema(kind: SchemaKind, value: &Value) -> Result<()> {
    let schema: Value = serde_json::from_str(kind.schema_text())
        .map_err(|err| KioError::schema(err.to_string()))?;
    let validator =
        jsonschema::validator_for(&schema).map_err(|err| KioError::schema(err.to_string()))?;
    validator
        .validate(value)
        .map_err(|err| KioError::schema(format!("{}: {err}", kind.name())))
}

/// Select operational config sections without making unknown root keys valid.
/// Known names come from the canonical schema so every consumer shares its
/// closed namespace while ignoring known sections it does not consume.
pub fn project_config_sections(value: &Value, selected: &[&str]) -> Result<Value> {
    let schema: Value = serde_json::from_str(SchemaKind::Config.schema_text())
        .map_err(|err| KioError::schema(err.to_string()))?;
    let known = schema
        .get("properties")
        .and_then(Value::as_object)
        .ok_or_else(|| KioError::schema("config schema has no root properties"))?;
    let mut projected = value.clone();
    if let Some(root) = projected.as_object_mut() {
        root.retain(|name, _| selected.contains(&name.as_str()) || !known.contains_key(name));
    }
    Ok(projected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn config_projection_ignores_known_unused_sections_and_preserves_unknown_names() {
        let value =
            json!({"budget": {"monthly_usd_cap": 1}, "search": {"default_mode": "invalid-unused"}});
        let projected = project_config_sections(&value, &["budget"]).unwrap();
        assert_eq!(projected, json!({"budget": {"monthly_usd_cap": 1}}));
        validate_json_schema(SchemaKind::Config, &projected).unwrap();
        for invalid in [
            json!({"budegt": {"monthly_usd_cap": 1}}),
            json!({"budget": {"monthly_usd_capp": 1}}),
            json!({"budget": "bad"}),
        ] {
            let projected = project_config_sections(&invalid, &["budget"]).unwrap();
            assert_eq!(projected, invalid);
            assert!(validate_json_schema(SchemaKind::Config, &projected).is_err());
        }
    }
}
