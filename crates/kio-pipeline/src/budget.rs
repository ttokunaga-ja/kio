//! Cost guardrail and budget contracts.
//!
//! The device-global charge/reservation ledger itself (formerly `CostLedger` /
//! `ReservationLedger` here, a JSONL charge file plus a sibling
//! reservations/reclaimed JSONL pair — see 10-operations.md §11.7's rename
//! table for the retired on-disk names) was retired 2026-07-21 in favor of
//! `kio_pipeline::ledger` (`cost-ledger.sqlite`, 04-pipeline.md §5.4/§5.8).
//! This module now holds only the ledger-storage-independent pieces: budget
//! config parsing/defaults and the pure cap-arithmetic decision function.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::task::TaskType;
use crate::{PipelineError, Result};

pub const DEFAULT_DEVICE_MONTHLY_USD_CAP: f64 = 50.0;

/// F5 defaults (docs/04 §5.4). When `[budget]` omits these keys the behavior is
/// the historical one: a hard pause at the cap, warning once spend reaches 80% of
/// a cap.
pub const DEFAULT_WARN_AT_PERCENT: u8 = 80;
pub const DEFAULT_HARD_STOP: bool = true;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetConfig {
    pub monthly_usd_cap: f64,
    pub warn_at_percent: u8,
    pub hard_stop: bool,
    pub per_adapter: BTreeMap<String, f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetCapKind {
    Device,
    Folder,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetEstimate {
    pub scope_id: String,
    pub task_type: TaskType,
    pub estimated_usd: f64,
    pub adapter_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetDecision {
    pub allowed: bool,
    pub cap_kind: Option<BudgetCapKind>,
    pub remaining_usd: f64,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetCaps {
    pub device_monthly_usd_cap: f64,
    pub folder_monthly_usd_cap: Option<f64>,
    pub device_per_adapter: BTreeMap<String, f64>,
    // QA11/QA12 (step4b-contract-tests-p3a.md §D, arbitration #2): folder-layer
    // `[budget.per_adapter]` does not exist as a concept (04 §5.4 L768 —
    // `per_adapter` is device-layer only, folder cap is total-only) — there is
    // deliberately no `folder_per_adapter` field here for any code to read.
    // `read_budget_config` rejects a non-empty `[budget.per_adapter]` on the
    // FOLDER path outright (`KIO-E-CONFIG-SCHEMA-001`) rather than parsing it
    // into a value nothing may act on.
    /// F5: `false` = soft-stop (record the charge and continue over cap); `true`
    /// (default) = hard pause at the cap. The folder `.kio/config.toml` value
    /// overrides the device value when present.
    pub hard_stop: bool,
    /// F5: emit a non-blocking warning once device or folder spend reaches this
    /// percentage of its cap. Default 80. Folder overrides device when present.
    pub warn_at_percent: u8,
}

#[must_use]
pub fn evaluate_budget_with_caps(
    estimate: &BudgetEstimate,
    device_remaining_usd: f64,
    folder_remaining_usd: Option<f64>,
    override_budget: bool,
) -> BudgetDecision {
    if override_budget {
        return BudgetDecision {
            allowed: true,
            cap_kind: None,
            remaining_usd: f64::INFINITY,
            warning: Some("budget override active".to_owned()),
        };
    }
    let (effective_remaining, cap_kind) = match folder_remaining_usd {
        Some(folder) if folder <= device_remaining_usd => (folder, Some(BudgetCapKind::Folder)),
        _ => (device_remaining_usd, Some(BudgetCapKind::Device)),
    };
    let allowed = estimate.estimated_usd <= effective_remaining;
    BudgetDecision {
        allowed,
        cap_kind: if allowed { None } else { cap_kind },
        remaining_usd: effective_remaining,
        warning: (!allowed).then(|| "budget cap exceeded; new tasks are paused".to_owned()),
    }
}

#[must_use]
pub fn utc_month(timestamp: &str) -> String {
    timestamp.get(0..7).unwrap_or("1970-01").to_owned()
}

#[must_use]
pub fn estimate_local_baseline_cost(size_bytes: u64) -> f64 {
    if size_bytes == 0 {
        0.0
    } else {
        size_bytes as f64 / 1_000_000.0 * 0.01
    }
}

pub fn read_budget_policy(
    device_config_path: impl AsRef<Path>,
    folder_config_path: impl AsRef<Path>,
) -> Result<BudgetCaps> {
    let device = read_budget_config(device_config_path)?;
    let folder_path_display = folder_config_path.as_ref().display().to_string();
    let folder = read_budget_config(folder_config_path)?;
    // QA12 (step4b-contract-tests-p3a.md §D, arbitration #2): folder
    // `.kio/config.toml` does not define `[budget.per_adapter]` (04 §5.4
    // L768) — reject it as a config schema error rather than silently
    // dropping it (the earlier behavior QA11 fixes: it used to be parsed and
    // fed into the enqueue-time pre-check, narrowing remaining budget for a
    // constraint that does not exist at the folder layer).
    if !folder.per_adapter.is_empty() {
        return Err(PipelineError::Schema(format!(
            "budget.per_adapter is not defined for folder config (device-layer only, 04 §5.4) \
             at {folder_path_display}: {:?}",
            folder.per_adapter.keys().collect::<Vec<_>>()
        )));
    }
    Ok(BudgetCaps {
        device_monthly_usd_cap: device
            .monthly_usd_cap
            .unwrap_or(DEFAULT_DEVICE_MONTHLY_USD_CAP),
        folder_monthly_usd_cap: folder.monthly_usd_cap,
        device_per_adapter: device.per_adapter,
        // F5: the more specific folder config overrides the device config; absent
        // both, the historical default (hard pause / warn at 80%).
        hard_stop: folder
            .hard_stop
            .or(device.hard_stop)
            .unwrap_or(DEFAULT_HARD_STOP),
        warn_at_percent: folder
            .warn_at_percent
            .or(device.warn_at_percent)
            .unwrap_or(DEFAULT_WARN_AT_PERCENT),
    })
}

/// F5: the non-blocking budget warning, or `None` when neither cap has crossed
/// `warn_at_percent`. Reports the layer (`device`/`folder`) at the higher
/// percentage. A zero or absent cap is skipped (a zero cap already hard-pauses).
#[must_use]
pub fn budget_warning(caps: &BudgetCaps, device_spent: f64, folder_spent: f64) -> Option<String> {
    let threshold = f64::from(caps.warn_at_percent);
    let mut worst: Option<(f64, &'static str)> = None;
    let mut consider = |spent: f64, cap: f64, layer: &'static str| {
        if cap > 0.0 {
            let pct = spent / cap * 100.0;
            if pct >= threshold && worst.is_none_or(|(best, _)| pct > best) {
                worst = Some((pct, layer));
            }
        }
    };
    consider(device_spent, caps.device_monthly_usd_cap, "device");
    if let Some(folder_cap) = caps.folder_monthly_usd_cap {
        consider(folder_spent, folder_cap, "folder");
    }
    worst.map(|(pct, layer)| format!("{layer} budget at {}% of cap", pct.round() as u64))
}

#[derive(Debug, Default)]
struct ParsedBudgetConfig {
    monthly_usd_cap: Option<f64>,
    per_adapter: BTreeMap<String, f64>,
    hard_stop: Option<bool>,
    warn_at_percent: Option<u8>,
}

fn read_budget_config(config_path: impl AsRef<Path>) -> Result<ParsedBudgetConfig> {
    let path = config_path.as_ref();
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ParsedBudgetConfig::default());
        }
        Err(err) => {
            return Err(PipelineError::Io {
                path: path.display().to_string(),
                message: err.to_string(),
            });
        }
    };
    let value: toml::Value =
        toml::from_str(&text).map_err(|err| PipelineError::Schema(err.to_string()))?;
    // Defaults apply only to absent, valid keys. This reader is also used after
    // operation-specific startup composition, so it owns the consumed cost
    // policy validation instead of relying on unrelated command validation.
    let json_value =
        serde_json::to_value(&value).map_err(|err| PipelineError::Schema(err.to_string()))?;
    let projected = kio_core::schema::project_config_sections(&json_value, &["budget"])
        .map_err(|err| PipelineError::Schema(err.to_string()))?;
    kio_core::schema::validate_json_schema(kio_core::schema::SchemaKind::Config, &projected)
        .map_err(|err| PipelineError::Schema(err.to_string()))?;
    let budget = value.get("budget");
    let per_adapter: BTreeMap<String, f64> = budget
        .and_then(|value| value.get("per_adapter"))
        .and_then(toml::Value::as_table)
        .map(|table| {
            table
                .iter()
                .filter_map(|(key, value)| {
                    value
                        .as_float()
                        .or_else(|| value.as_integer().map(|value| value as f64))
                        .map(|cap| (key.clone(), cap))
                })
                .collect()
        })
        .unwrap_or_default();
    let monthly_usd_cap = budget
        .and_then(|value| value.get("monthly_usd_cap"))
        .and_then(toml::Value::as_float)
        .or_else(|| {
            budget
                .and_then(|value| value.get("monthly_usd_cap"))
                .and_then(toml::Value::as_integer)
                .map(|value| value as f64)
        });
    // M8: non-negative guard on budget caps (defense-in-depth behind the
    // config.schema.json `minimum: 0` constraint, 10 §11 / 06 §10). A negative cap
    // is nonsensical and would silently invert the budget arithmetic; reject it
    // (exit 2 KIO-E-CONFIG-SCHEMA-001 via `pipeline_to_kio`).
    if let Some(cap) = monthly_usd_cap
        && cap < 0.0
    {
        return Err(PipelineError::Schema(format!(
            "budget.monthly_usd_cap must be non-negative at {}: {cap}",
            path.display()
        )));
    }
    for (adapter, cap) in &per_adapter {
        if *cap < 0.0 {
            return Err(PipelineError::Schema(format!(
                "budget.per_adapter.{adapter} must be non-negative at {}: {cap}",
                path.display()
            )));
        }
        // CL61 (04 §5.4 L768): "設定キー名 = adapter_kind と同一 enum: markdownize /
        // embedding. enum 外の未知キーは schema error" — the per_adapter
        // cap is device-layer-only (`crate::ledger::ops::check_then_reserve`'s
        // third condition; folder cap stays total-only), so its key namespace is
        // exactly `crate::ledger::ops::PER_ADAPTER_KIND_ENUM`, not the broader
        // `kio_adapter::types::AdapterKind` set (which also has `prepare` /
        // `rerank` — neither is budget-capped).
        if !crate::ledger::ops::is_valid_per_adapter_key(adapter) {
            return Err(PipelineError::Schema(format!(
                "budget.per_adapter key must be one of {:?} at {}: {adapter}",
                crate::ledger::ops::PER_ADAPTER_KIND_ENUM,
                path.display()
            )));
        }
    }
    // F5: `hard_stop` (bool) and `warn_at_percent` (0..=100) are documented in
    // docs/04 §5.4. `None` = key absent → the caller applies the default.
    let hard_stop = budget
        .and_then(|value| value.get("hard_stop"))
        .and_then(toml::Value::as_bool);
    let warn_at_percent = match budget
        .and_then(|value| value.get("warn_at_percent"))
        .and_then(toml::Value::as_integer)
    {
        Some(percent) => {
            if !(0..=100).contains(&percent) {
                return Err(PipelineError::Schema(format!(
                    "budget.warn_at_percent must be between 0 and 100 at {}: {percent}",
                    path.display()
                )));
            }
            Some(percent as u8)
        }
        None => None,
    };
    Ok(ParsedBudgetConfig {
        monthly_usd_cap,
        per_adapter,
        hard_stop,
        warn_at_percent,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consumed_budget_policy_rejects_invalid_fields_before_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let device = dir.path().join("device.toml");
        let folder = dir.path().join("folder.toml");
        for invalid in [
            "[budget]\nmonthly_usd_capp = 1\n",
            "budget = 'bad'\n",
            "[budget]\nhard_stop = 'bad'\n",
            "[budget]\nmonthly_usd_cap = 'bad'\n",
            "[budget]\nwarn_at_percent = 'bad'\n",
            "[budget.per_adapter]\nembedding = 'bad'\n",
            "[budegt]\nmonthly_usd_cap = 1\n",
        ] {
            for selected in [&device, &folder] {
                std::fs::write(selected, invalid).unwrap();
                assert!(
                    matches!(
                        read_budget_policy(&device, &folder),
                        Err(PipelineError::Schema(_))
                    ),
                    "consumed policy silently defaulted: {invalid}"
                );
                std::fs::remove_file(selected).unwrap();
            }
        }
        std::fs::write(
            &device,
            "[budget]\nmonthly_usd_cap = 1\n[search]\ndefault_mode = 'unused-invalid'\n",
        )
        .unwrap();
        assert_eq!(
            read_budget_policy(&device, &folder)
                .unwrap()
                .device_monthly_usd_cap,
            1.0
        );
    }

    #[test]
    fn placeholder_budget_cap_kind_serializes() {
        let value = serde_json::to_value(BudgetCapKind::Folder).expect("serialize cap kind");
        assert_eq!(value, "folder");
    }

    #[test]
    fn two_layer_budget_uses_min_remaining_and_override() {
        let estimate = BudgetEstimate {
            scope_id: "scope".to_owned(),
            task_type: TaskType::Markdownize,
            estimated_usd: 12.0,
            adapter_id: Some("mistral".to_owned()),
        };
        let denied = evaluate_budget_with_caps(&estimate, 50.0, Some(10.0), false);
        assert!(!denied.allowed);
        assert_eq!(denied.cap_kind, Some(BudgetCapKind::Folder));
        assert_eq!(denied.remaining_usd, 10.0);

        let allowed = evaluate_budget_with_caps(&estimate, 50.0, Some(10.0), true);
        assert!(allowed.allowed);
        assert_eq!(allowed.cap_kind, None);
    }

    // F5: the pure warning crosses at `warn_at_percent` of whichever cap is higher;
    // below the threshold it is silent.
    #[test]
    fn f5_budget_warning_crosses_threshold_and_reports_worst_layer() {
        let caps = BudgetCaps {
            device_monthly_usd_cap: 100.0,
            folder_monthly_usd_cap: Some(10.0),
            device_per_adapter: BTreeMap::new(),
            hard_stop: true,
            warn_at_percent: 80,
        };
        // Both below 80% → no warning.
        assert_eq!(budget_warning(&caps, 50.0, 5.0), None);
        // Device at 85% → warn device.
        assert_eq!(
            budget_warning(&caps, 85.0, 5.0).as_deref(),
            Some("device budget at 85% of cap")
        );
        // Folder at 95% while device at 85% → report the higher (folder).
        assert_eq!(
            budget_warning(&caps, 85.0, 9.5).as_deref(),
            Some("folder budget at 95% of cap")
        );
    }

    // F5: absent keys resolve to the historical default (hard pause / warn 80), and
    // the more specific folder config overrides the device config.
    #[test]
    fn f5_hard_stop_and_warn_at_percent_parse_and_default() {
        let dir = tempfile::tempdir().unwrap();
        let device = dir.path().join("device.toml");
        let folder = dir.path().join("folder.toml");

        // Both files absent → defaults.
        let defaults = read_budget_policy(&device, &folder).unwrap();
        assert!(defaults.hard_stop);
        assert_eq!(defaults.warn_at_percent, DEFAULT_WARN_AT_PERCENT);

        // Device sets soft-stop + warn 70; folder overrides warn to 90.
        std::fs::write(
            &device,
            "[budget]\nhard_stop = false\nwarn_at_percent = 70\n",
        )
        .unwrap();
        std::fs::write(&folder, "[budget]\nwarn_at_percent = 90\n").unwrap();
        let resolved = read_budget_policy(&device, &folder).unwrap();
        assert!(!resolved.hard_stop, "device hard_stop=false must apply");
        assert_eq!(resolved.warn_at_percent, 90, "folder overrides device");
    }

    #[test]
    fn f5_warn_at_percent_out_of_range_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let device = dir.path().join("device.toml");
        let folder = dir.path().join("folder.toml");
        std::fs::write(&device, "[budget]\nwarn_at_percent = 150\n").unwrap();
        let err = read_budget_policy(&device, &folder).unwrap_err();
        assert!(matches!(err, PipelineError::Schema(_)), "got {err:?}");
    }

    // QA12 (step4b-contract-tests-p3a.md §D, arbitration #2): folder
    // `.kio/config.toml` does not define `[budget.per_adapter]` (04 §5.4
    // L768 — device-layer only) — a folder config setting it is a schema
    // error, not a silently-ignored key. The identical key on the DEVICE
    // config still parses (regression check against `cl61_*` below).
    #[test]
    fn qa12_folder_per_adapter_is_a_schema_error() {
        let dir = tempfile::tempdir().unwrap();
        let device = dir.path().join("device.toml");
        let folder = dir.path().join("folder.toml");
        std::fs::write(&folder, "[budget.per_adapter]\nmarkdownize = 0.0\n").unwrap();
        let err = read_budget_policy(&device, &folder).unwrap_err();
        assert!(matches!(err, PipelineError::Schema(_)), "got {err:?}");
        assert!(err.to_string().contains("budget.per_adapter"));

        // The device-side equivalent is unaffected.
        std::fs::remove_file(&folder).unwrap();
        std::fs::write(&device, "[budget.per_adapter]\nmarkdownize = 0.0\n").unwrap();
        let ok = read_budget_policy(&device, &folder).unwrap();
        assert_eq!(ok.device_per_adapter.get("markdownize"), Some(&0.0));
    }

    // CL61 (04 §5.4 L768): `[budget.per_adapter]` keys are the closed
    // `markdownize`/`embedding` enum (device-layer-only per
    // `crate::ledger::ops::check_then_reserve`'s third condition); an unknown key
    // is a schema error (exit 2 KIO-E-CONFIG-SCHEMA-001 via `pipeline_to_kio`'s
    // catch-all `PipelineError::Schema` mapping).
    #[test]
    fn cl61_per_adapter_key_enum_is_validated_on_read() {
        let dir = tempfile::tempdir().unwrap();
        let device = dir.path().join("device.toml");
        let folder = dir.path().join("folder.toml");

        // Both valid keys parse.
        std::fs::write(
            &device,
            "[budget.per_adapter]\nmarkdownize = 30.0\nembedding = 15.0\n",
        )
        .unwrap();
        let ok = read_budget_policy(&device, &folder).unwrap();
        assert_eq!(ok.device_per_adapter.get("markdownize"), Some(&30.0));
        assert_eq!(ok.device_per_adapter.get("embedding"), Some(&15.0));

        // The legacy JSONL-era "markdown" key (no trailing -ize) is now outside
        // the enum and must be rejected, not silently accepted under the wrong
        // name (the "markdown"→"markdownize" rename this fix closes).
        std::fs::write(&device, "[budget.per_adapter]\nmarkdown = 30.0\n").unwrap();
        let err = read_budget_policy(&device, &folder).unwrap_err();
        assert!(matches!(err, PipelineError::Schema(_)), "got {err:?}");

        // Any other unknown key is likewise rejected.
        std::fs::write(&device, "[budget.per_adapter]\nunknown_kind = 5.0\n").unwrap();
        let err2 = read_budget_policy(&device, &folder).unwrap_err();
        assert!(matches!(err2, PipelineError::Schema(_)), "got {err2:?}");
    }
}
