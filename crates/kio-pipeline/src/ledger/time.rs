//! UTC epoch-millisecond time helpers for `cost-ledger.sqlite` (10-operations.md
//! §12.4: the SQLite store's internal time columns are the one exception to the
//! ISO8601+Z convention — UTC epoch milliseconds INTEGER, so SQL comparisons and
//! deadline arithmetic stay native). All calendar math (`month` derivation,
//! "previous UTC calendar month" pruning boundaries — 04 §5.4) is done in UTC.
//!
//! `civil_from_days` / `days_from_civil` are Howard Hinnant's well-known
//! public-domain civil-calendar algorithm, already used (in a private, crate-local
//! form) by `kio_core::scope`. That copy is not `pub`, and `kio-core/src/scope.rs`
//! is out of bounds for this change (a parallel purge/scope edit is in flight), so
//! the ~10-line algorithm is duplicated here rather than exposed cross-crate.

use std::time::{SystemTime, UNIX_EPOCH};

/// Current UTC time as epoch milliseconds from the real system clock.
#[must_use]
pub fn now_millis() -> i64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    now.as_millis() as i64
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096).div_euclid(365);
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2).div_euclid(153);
    let day = doy - (153 * mp + 2).div_euclid(5) + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 }.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2).div_euclid(5) + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The `'YYYY-MM'` UTC calendar month containing `epoch_ms` (04 §5.4
/// `cost_ledger.month`: "derived from `recorded_at`'s UTC calendar month").
#[must_use]
pub fn utc_month_of(epoch_ms: i64) -> String {
    let days = epoch_ms.div_euclid(86_400_000);
    let (year, month, _day) = civil_from_days(days);
    format!("{year:04}-{month:02}")
}

/// UTC epoch milliseconds at `YYYY-MM-01T00:00:00Z` for the given calendar month.
#[must_use]
pub fn month_start_millis(year: i64, month: i64) -> i64 {
    days_from_civil(year, month, 1) * 86_400_000
}

/// Parse a `'YYYY-MM'` string into `(year, month)`. `None` if malformed or the
/// month is out of `01..=12` (mirrors the `cost_ledger.month` / `batch_requests`
/// CHECK constraint's GLOB + range rule, 04 §5.4 DDL).
#[must_use]
pub fn parse_month(value: &str) -> Option<(i64, i64)> {
    let bytes = value.as_bytes();
    if bytes.len() != 7 || bytes[4] != b'-' {
        return None;
    }
    if !bytes[0..4].iter().all(u8::is_ascii_digit) || !bytes[5..7].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let year: i64 = value[0..4].parse().ok()?;
    let month: i64 = value[5..7].parse().ok()?;
    (1..=12).contains(&month).then_some((year, month))
}

/// The epoch-ms boundary for "the current UTC calendar month has not yet
/// started" — i.e. the start of the current month containing `now_ms` (04 §5.4
/// terminal device-row pruning: `completed_at` must be strictly before this to
/// count as "the previous month or earlier").
#[must_use]
pub fn current_month_start_millis(now_ms: i64) -> i64 {
    let (year, month) = parse_month(&utc_month_of(now_ms)).expect("utc_month_of always YYYY-MM");
    month_start_millis(year, month)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_month_of_matches_civil_calendar() {
        // 2026-07-21T00:00:00Z boundary sanity (matches this session's fixed date).
        let ms = month_start_millis(2026, 7);
        assert_eq!(utc_month_of(ms), "2026-07");
        assert_eq!(utc_month_of(ms - 1), "2026-06");
        // July has 31 days: +31 days from July 1 lands exactly on August 1.
        assert_eq!(utc_month_of(ms + 86_400_000 * 31), "2026-08");
    }

    #[test]
    fn parse_month_rejects_malformed_and_out_of_range() {
        assert_eq!(parse_month("2026-07"), Some((2026, 7)));
        assert_eq!(parse_month("2026-13"), None);
        assert_eq!(parse_month("2026-00"), None);
        assert_eq!(parse_month("26-07"), None);
        assert_eq!(parse_month("2026-7"), None);
        assert_eq!(parse_month("2026/07"), None);
    }

    #[test]
    fn current_month_start_is_idempotent_within_the_month() {
        let start = month_start_millis(2026, 7);
        assert_eq!(current_month_start_millis(start), start);
        assert_eq!(current_month_start_millis(start + 86_400_000 * 20), start);
    }
}
