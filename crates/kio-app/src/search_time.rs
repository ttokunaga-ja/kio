//! Contract-frozen Step 4 time-travel search primitives.
//!
//! This module deliberately contains only selector, cursor, and frozen-cutoff
//! logic. CAS-backed binding planning lives in `search_history`.

use std::fmt;
use std::str::FromStr;

use kio_core::scope::{format_utc_seconds, parse_utc_seconds};
use kio_core::{ExitCode, KioError, Result};
use serde::de::Error as DeError;
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// A positive, overflow-checked CLI duration, canonicalized to whole seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PositiveDuration {
    seconds: u64,
}

impl PositiveDuration {
    /// Parse exactly `<positive integer><s|m|h|d|w>`.
    pub fn parse(value: &str) -> Result<Self> {
        let Some((unit_index, unit)) = value.char_indices().next_back() else {
            return Err(duration_usage_error());
        };
        let digits = &value[..unit_index];
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(duration_usage_error());
        }
        let amount = digits.parse::<u64>().map_err(|_| duration_usage_error())?;
        if amount == 0 {
            return Err(duration_usage_error());
        }
        let multiplier = match unit {
            's' => 1,
            'm' => 60,
            'h' => 60 * 60,
            'd' => 24 * 60 * 60,
            'w' => 7 * 24 * 60 * 60,
            _ => return Err(duration_usage_error()),
        };
        let seconds = amount
            .checked_mul(multiplier)
            .ok_or_else(duration_usage_error)?;
        Ok(Self { seconds })
    }

    #[must_use]
    pub const fn seconds(self) -> u64 {
        self.seconds
    }

    /// Canonical query/cursor spelling.  Equivalent inputs such as `1m` and
    /// `60s` therefore have the same identity.
    #[must_use]
    pub fn canonical(self) -> String {
        format!("{}s", self.seconds)
    }
}

impl fmt::Display for PositiveDuration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}s", self.seconds)
    }
}

impl FromStr for PositiveDuration {
    type Err = KioError;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        Self::parse(value)
    }
}

fn duration_usage_error() -> KioError {
    KioError::invalid_usage("--since must be a positive integer followed by s, m, h, d, or w")
}

/// The one effective Step 4 time selector.
///
/// Its custom wire representation is the canonical `time_travel` JSON object:
/// `{}`, `{\"at\":...}`, `{\"all_history\":true}`,
/// `{\"include_deleted\":true}`, or
/// `{\"all_history\":true,\"since\":\"<seconds>s\"}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub enum TimeSelector {
    #[default]
    Current,
    At(String),
    AllHistory,
    IncludeDeleted,
    Since(PositiveDuration),
}

impl TimeSelector {
    #[must_use]
    pub fn at(&self) -> Option<&str> {
        match self {
            Self::At(value) => Some(value),
            _ => None,
        }
    }

    #[must_use]
    pub const fn all_history(&self) -> bool {
        matches!(self, Self::AllHistory | Self::Since(_))
    }

    #[must_use]
    pub const fn include_deleted(&self) -> bool {
        matches!(self, Self::IncludeDeleted)
    }

    #[must_use]
    pub const fn since(&self) -> Option<PositiveDuration> {
        match self {
            Self::Since(duration) => Some(*duration),
            _ => None,
        }
    }

    /// The exact object to place in query-hash input and a v2 cursor.
    pub fn canonical_value(&self) -> Result<Value> {
        serde_json::to_value(self).map_err(|error| KioError::schema(error.to_string()))
    }
}

impl Serialize for TimeSelector {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let length = match self {
            Self::Current => 0,
            Self::Since(_) => 2,
            _ => 1,
        };
        let mut map = serializer.serialize_map(Some(length))?;
        match self {
            Self::Current => {}
            Self::At(value) => map.serialize_entry("at", value)?,
            Self::AllHistory => map.serialize_entry("all_history", &true)?,
            Self::IncludeDeleted => map.serialize_entry("include_deleted", &true)?,
            Self::Since(duration) => {
                map.serialize_entry("all_history", &true)?;
                map.serialize_entry("since", &duration.canonical())?;
            }
        }
        map.end()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TimeSelectorWire {
    at: Option<String>,
    all_history: Option<bool>,
    include_deleted: Option<bool>,
    since: Option<String>,
}

impl<'de> Deserialize<'de> for TimeSelector {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = TimeSelectorWire::deserialize(deserializer)?;
        // Cursor/query JSON must already have one exact canonical shape.  In
        // particular, accepting `7d`, a missing implied all_history, or explicit
        // false values would decode signed bytes into a different query identity.
        match (wire.at, wire.all_history, wire.include_deleted, wire.since) {
            (None, None, None, None) => Ok(Self::Current),
            (Some(value), None, None, None) if !value.is_empty() => Ok(Self::At(value)),
            (None, Some(true), None, None) => Ok(Self::AllHistory),
            (None, None, Some(true), None) => Ok(Self::IncludeDeleted),
            (None, Some(true), None, Some(raw)) => {
                let duration = PositiveDuration::parse(&raw).map_err(D::Error::custom)?;
                if raw != duration.canonical() {
                    return Err(D::Error::custom(
                        "time_travel.since must use canonical whole seconds",
                    ));
                }
                Ok(Self::Since(duration))
            }
            _ => Err(D::Error::custom(
                "time_travel must use one canonical selector shape",
            )),
        }
    }
}

/// Raw CLI flag state before exclusivity and duration canonicalization.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TimeSelectorFlags {
    pub at: Option<String>,
    pub all_history: bool,
    pub include_deleted: bool,
    pub since: Option<String>,
}

impl TimeSelectorFlags {
    #[must_use]
    pub fn is_explicit(&self) -> bool {
        self.at.is_some() || self.all_history || self.include_deleted || self.since.is_some()
    }

    /// Enforce the single-effective-selector contract.  The sole accepted
    /// redundancy is `--all-history --since D`, because `--since` implies it.
    pub fn canonicalize(&self) -> Result<TimeSelector> {
        if self.at.is_some() && (self.all_history || self.include_deleted || self.since.is_some()) {
            return Err(selector_exclusivity_error());
        }
        if self.include_deleted && (self.all_history || self.since.is_some()) {
            return Err(selector_exclusivity_error());
        }
        if let Some(value) = &self.at {
            if value.is_empty() {
                return Err(KioError::invalid_usage("--at requires a non-empty value"));
            }
            return Ok(TimeSelector::At(value.clone()));
        }
        if self.include_deleted {
            return Ok(TimeSelector::IncludeDeleted);
        }
        if let Some(value) = &self.since {
            return Ok(TimeSelector::Since(PositiveDuration::parse(value)?));
        }
        if self.all_history {
            return Ok(TimeSelector::AllHistory);
        }
        Ok(TimeSelector::Current)
    }
}

fn selector_exclusivity_error() -> KioError {
    KioError::invalid_usage("only one effective time-travel selector may be used")
}

/// Inherit a cursor's selector when the replay supplied no selector flags, or
/// require an explicitly repeated selector to be canonically identical.
pub fn reconcile_cursor_selector(
    explicit: Option<&TimeSelector>,
    frozen: &TimeSelector,
) -> Result<TimeSelector> {
    match explicit {
        None => Ok(frozen.clone()),
        Some(requested) if requested == frozen => Ok(frozen.clone()),
        Some(requested) => Err(cursor_error(
            "search cursor time_travel selector mismatch",
            serde_json::json!({
                "expected": frozen.canonical_value()?,
                "actual": requested.canonical_value()?,
            }),
        )),
    }
}

/// Validate the required relationship between a v2 cursor selector and its
/// frozen page-1 cutoff.  The cutoff value itself is never recomputed on replay.
pub fn validate_cursor_cutoff(selector: &TimeSelector, cutoff: Option<&str>) -> Result<()> {
    match (selector.since(), cutoff) {
        (Some(_), Some(value)) if parse_canonical_utc_seconds(value).is_some() => Ok(()),
        (Some(_), Some(_)) => Err(cursor_error(
            "search cursor since_cutoff is not canonical UTC seconds",
            serde_json::json!({}),
        )),
        (Some(_), None) => Err(cursor_error(
            "search cursor is missing since_cutoff",
            serde_json::json!({}),
        )),
        (None, Some(_)) => Err(cursor_error(
            "search cursor has since_cutoff without --since",
            serde_json::json!({}),
        )),
        (None, None) => Ok(()),
    }
}

fn parse_canonical_utc_seconds(value: &str) -> Option<i64> {
    let seconds = parse_utc_seconds(value)?;
    (format_utc_seconds(seconds) == value).then_some(seconds)
}

fn cursor_error(message: &str, context: Value) -> KioError {
    KioError::new(
        "KIO-E-SEARCH-CURSOR-001",
        message,
        context,
        ExitCode::InvalidUsage,
    )
}

/// Subtract a canonical duration from page 1's fixed Unix-second clock reading.
pub fn since_cutoff_seconds(now_seconds: i64, duration: PositiveDuration) -> Result<i64> {
    let duration = i64::try_from(duration.seconds())
        .map_err(|_| KioError::invalid_usage("--since duration is too large"))?;
    now_seconds
        .checked_sub(duration)
        .ok_or_else(|| KioError::invalid_usage("--since cutoff overflows the supported time range"))
}

/// Calculate the one page-1 cutoff for a selector from a fixed canonical UTC
/// timestamp.  Non-`--since` selectors have no cutoff.
pub fn since_cutoff_utc(selector: &TimeSelector, page_one_now_utc: &str) -> Result<Option<String>> {
    let Some(duration) = selector.since() else {
        return Ok(None);
    };
    let now = parse_canonical_utc_seconds(page_one_now_utc)
        .ok_or_else(|| KioError::schema("page-1 clock must be canonical UTC seconds"))?;
    let cutoff_seconds = since_cutoff_seconds(now, duration)?;
    let cutoff = format_utc_seconds(cutoff_seconds);
    // `format_utc_seconds` supports the full i64 calendar, while the persisted
    // contract intentionally uses a fixed four-digit RFC3339 year.
    if parse_utc_seconds(&cutoff) != Some(cutoff_seconds) {
        return Err(KioError::invalid_usage(
            "--since cutoff is outside the supported UTC timestamp range",
        ));
    }
    Ok(Some(cutoff))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct4_timetravel_001_duration_grammar_and_canonical_seconds() {
        let vectors = [
            ("1s", 1),
            ("2m", 120),
            ("3h", 10_800),
            ("7d", 604_800),
            ("2w", 1_209_600),
        ];
        for (raw, seconds) in vectors {
            let duration = PositiveDuration::parse(raw).unwrap();
            assert_eq!(duration.seconds(), seconds);
            assert_eq!(duration.canonical(), format!("{seconds}s"));
        }
        for invalid in ["", "0d", "7", "7.0d", "-7d", "+7d", "7D", "today"] {
            let error = PositiveDuration::parse(invalid).unwrap_err();
            assert_eq!(error.error_code(), "KIO-E-CONFIG-USAGE-001");
            assert_eq!(error.exit_code(), ExitCode::InvalidUsage);
        }
        assert!(PositiveDuration::parse("18446744073709551615w").is_err());
    }

    #[test]
    fn ct4_timetravel_001_selector_exclusivity_and_since_implication() {
        let since = TimeSelectorFlags {
            all_history: true,
            since: Some("7d".to_owned()),
            ..TimeSelectorFlags::default()
        }
        .canonicalize()
        .unwrap();
        assert_eq!(
            since,
            TimeSelector::Since(PositiveDuration { seconds: 604_800 })
        );
        assert_eq!(
            since.canonical_value().unwrap(),
            serde_json::json!({"all_history": true, "since": "604800s"})
        );

        let conflicts = [
            TimeSelectorFlags {
                at: Some("HEAD".to_owned()),
                all_history: true,
                ..TimeSelectorFlags::default()
            },
            TimeSelectorFlags {
                include_deleted: true,
                since: Some("1d".to_owned()),
                ..TimeSelectorFlags::default()
            },
            TimeSelectorFlags {
                include_deleted: true,
                all_history: true,
                ..TimeSelectorFlags::default()
            },
        ];
        for flags in conflicts {
            assert_eq!(
                flags.canonicalize().unwrap_err().error_code(),
                "KIO-E-CONFIG-USAGE-001"
            );
        }
    }

    #[test]
    fn ct4_timetravel_005_cutoff_is_subtracted_once_at_seconds_precision() {
        let selector = TimeSelectorFlags {
            since: Some("7d".to_owned()),
            ..TimeSelectorFlags::default()
        }
        .canonicalize()
        .unwrap();
        assert_eq!(
            since_cutoff_utc(&selector, "2026-07-13T00:00:00Z").unwrap(),
            Some("2026-07-06T00:00:00Z".to_owned())
        );
        assert_eq!(
            since_cutoff_utc(&TimeSelector::AllHistory, "2026-07-13T00:00:00Z").unwrap(),
            None
        );
    }

    #[test]
    fn ct4_timetravel_006_cursor_inherits_or_requires_exact_selector() {
        let frozen = TimeSelector::Since(PositiveDuration { seconds: 604_800 });
        assert_eq!(reconcile_cursor_selector(None, &frozen).unwrap(), frozen);
        let equivalent = TimeSelectorFlags {
            since: Some("168h".to_owned()),
            ..TimeSelectorFlags::default()
        }
        .canonicalize()
        .unwrap();
        assert_eq!(
            reconcile_cursor_selector(Some(&equivalent), &frozen).unwrap(),
            frozen
        );
        let error =
            reconcile_cursor_selector(Some(&TimeSelector::AllHistory), &frozen).unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-SEARCH-CURSOR-001");

        validate_cursor_cutoff(&frozen, Some("2026-07-06T00:00:00Z")).unwrap();
        assert!(validate_cursor_cutoff(&frozen, None).is_err());
        assert!(
            validate_cursor_cutoff(&TimeSelector::AllHistory, Some("2026-07-06T00:00:00Z"))
                .is_err()
        );
    }

    #[test]
    fn cursor_deserialize_requires_canonical_since() {
        let canonical: TimeSelector = serde_json::from_value(serde_json::json!({
            "all_history": true,
            "since": "604800s"
        }))
        .unwrap();
        assert_eq!(canonical.since().unwrap().seconds(), 604_800);
        assert!(
            serde_json::from_value::<TimeSelector>(serde_json::json!({
                "all_history": true,
                "since": "7d"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<TimeSelector>(serde_json::json!({
                "since": "604800s"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<TimeSelector>(serde_json::json!({
                "all_history": false
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<TimeSelector>(serde_json::json!({
                "at": "HEAD",
                "unknown": true
            }))
            .is_err()
        );
    }
}
