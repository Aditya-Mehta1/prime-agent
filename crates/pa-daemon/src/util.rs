//! Small shared helpers.

use std::time::{SystemTime, UNIX_EPOCH};

/// 12-char hex display id (port of `formatSessionDisplayId`): the last 12
/// hex characters of a random UUID.
pub fn new_display_id() -> String {
    let normalized: String = uuid::Uuid::new_v4().simple().to_string().to_lowercase();
    normalized[(normalized.len() - 12)..].to_string()
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// RFC 3339 / ISO 8601 UTC timestamp, matching `new Date().toISOString()`.
pub fn now_iso() -> String {
    iso_from_unix_ms(now_ms())
}

/// RFC 3339 UTC timestamp from epoch milliseconds (no external time crate;
/// civil-from-days algorithm from Howard Hinnant, used by chrono).
pub fn iso_from_unix_ms(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let millis = (ms % 1000) as u32;
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_matches_known_dates() {
        assert_eq!(iso_from_unix_ms(0), "1970-01-01T00:00:00.000Z");
        // 2026-09-16T00:00:00Z = 1789555200
        assert_eq!(
            iso_from_unix_ms(1_789_516_800_000),
            "2026-09-16T00:00:00.000Z"
        );
        // Leap-day boundary: 2024-02-29T23:59:59.999Z = 1709251199
        assert_eq!(
            iso_from_unix_ms(1_709_251_199_999),
            "2024-02-29T23:59:59.999Z"
        );
    }
}
