//! UTC timestamps shared by plans and session records.

/// Generate an ISO 8601 timestamp string (UTC, second precision).
pub fn now_iso_public() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    // Simple ISO format from Unix timestamp
    let (year, month, day, hour, min, sec) = unix_to_ymd_hms(secs);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        year, month, day, hour, min, sec
    )
}

/// Convert a Unix timestamp to (year, month, day, hour, min, sec) in UTC.
fn unix_to_ymd_hms(secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let days = secs / 86400;
    let rem = secs % 86400;
    let hour = rem / 3600;
    let min = (rem % 3600) / 60;
    let sec = rem % 60;

    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm)
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]

    // Hinnant's algorithm computes the year for the March-based year; the
    // civil year is one greater when the resulting month is Jan or Feb.
    let y = y as u64 + u64::from(m <= 2);

    (y, m, d, hour, min, sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── unix_to_ymd_hms date arithmetic ────────────────────────────────
    //
    // Expected values are cross-checked against Python's
    // `datetime.fromtimestamp(ts, tz=timezone.utc)`.

    #[test]
    fn unix_to_ymd_hms_epoch() {
        assert_eq!(unix_to_ymd_hms(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(unix_to_ymd_hms(1), (1970, 1, 1, 0, 0, 1));
        assert_eq!(unix_to_ymd_hms(86400), (1970, 1, 2, 0, 0, 0));
    }

    #[test]
    fn unix_to_ymd_hms_year_boundary() {
        // 1999-12-31 23:59:59 UTC
        assert_eq!(unix_to_ymd_hms(946684799), (1999, 12, 31, 23, 59, 59));
        // 2000-01-01 00:00:00 UTC
        assert_eq!(unix_to_ymd_hms(946684800), (2000, 1, 1, 0, 0, 0));
        // 1970-12-31 23:59:59 UTC
        assert_eq!(unix_to_ymd_hms(31535999), (1970, 12, 31, 23, 59, 59));
        // 1971-01-01 00:00:00 UTC
        assert_eq!(unix_to_ymd_hms(31536000), (1971, 1, 1, 0, 0, 0));
    }

    #[test]
    fn unix_to_ymd_hms_leap_years() {
        // 2000 is a leap year (divisible by 400).
        assert_eq!(unix_to_ymd_hms(951782400), (2000, 2, 29, 0, 0, 0));
        assert_eq!(unix_to_ymd_hms(951868800), (2000, 3, 1, 0, 0, 0));
        // 2004 is a leap year.
        assert_eq!(unix_to_ymd_hms(1078012800), (2004, 2, 29, 0, 0, 0));
        assert_eq!(unix_to_ymd_hms(1078099200), (2004, 3, 1, 0, 0, 0));
        // 2024 is a leap year.
        assert_eq!(unix_to_ymd_hms(1709164800), (2024, 2, 29, 0, 0, 0));
        assert_eq!(unix_to_ymd_hms(1709251200), (2024, 3, 1, 0, 0, 0));
    }

    #[test]
    fn unix_to_ymd_hms_non_leap_years() {
        // 2023 is not a leap year — Feb has 28 days.
        assert_eq!(unix_to_ymd_hms(1677542400), (2023, 2, 28, 0, 0, 0));
        assert_eq!(unix_to_ymd_hms(1677628800), (2023, 3, 1, 0, 0, 0));
        // 2100 is not a leap year (divisible by 100 but not 400).
        assert_eq!(unix_to_ymd_hms(4107456000), (2100, 2, 28, 0, 0, 0));
        assert_eq!(unix_to_ymd_hms(4107542400), (2100, 3, 1, 0, 0, 0));
    }

    #[test]
    fn unix_to_ymd_hms_dst_adjacent() {
        // These are UTC timestamps around US DST transitions; the civil date
        // must be correct regardless of any local timezone.
        // 2024-03-10 08:00:00 UTC (US DST start day).
        assert_eq!(unix_to_ymd_hms(1710057600), (2024, 3, 10, 8, 0, 0));
        // 2024-11-03 06:00:00 UTC (US DST end day).
        assert_eq!(unix_to_ymd_hms(1730613600), (2024, 11, 3, 6, 0, 0));
    }

    #[test]
    fn unix_to_ymd_hms_2038_boundary() {
        // 2038-01-19 03:14:07 UTC — the classic 32-bit time_t overflow point.
        assert_eq!(unix_to_ymd_hms(2147483647), (2038, 1, 19, 3, 14, 7));
    }

    #[test]
    fn now_iso_round_trips_through_parser() {
        // now_iso produces "YYYY-MM-DDTHH:MM:SS" (UTC, second precision).
        // Parse it back and confirm the fields are internally consistent and
        // within a sane window of the current time.
        let iso = now_iso_public();
        assert_eq!(iso.len(), 19, "ISO string: {iso}");
        assert_eq!(iso.as_bytes()[10], b'T', "ISO string: {iso}");

        let year: u64 = iso[0..4].parse().unwrap();
        let month: u64 = iso[5..7].parse().unwrap();
        let day: u64 = iso[8..10].parse().unwrap();
        let hour: u64 = iso[11..13].parse().unwrap();
        let min: u64 = iso[14..16].parse().unwrap();
        let sec: u64 = iso[17..19].parse().unwrap();

        assert!((2000..=2100).contains(&year), "year out of range: {iso}");
        assert!((1..=12).contains(&month), "month out of range: {iso}");
        assert!((1..=31).contains(&day), "day out of range: {iso}");
        assert!(hour < 24, "hour out of range: {iso}");
        assert!(min < 60, "minute out of range: {iso}");
        assert!(sec < 60, "second out of range: {iso}");

        // The generated timestamp must be within a couple seconds of now.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let generated = unix_to_ymd_hms(now);
        let parsed = (year, month, day, hour, min, sec);
        // Allow the second to tick between the two calls.
        let diff = seconds_since_epoch(parsed).abs_diff(now);
        assert!(diff <= 2, "generated {parsed:?} vs now {generated:?}");
    }

    /// Convert a (year, month, day, hour, min, sec) UTC tuple back to unix
    /// seconds, using the same civil-date arithmetic as `unix_to_ymd_hms`.
    fn seconds_since_epoch((y, m, d, h, min, s): (u64, u64, u64, u64, u64, u64)) -> u64 {
        // Days since 1970-01-01 via Howard Hinnant's algorithm (inverse).
        let y = y as i64;
        let m = m as i64;
        let d = d as i64;
        let yoe = if m <= 2 { y - 1 } else { y };
        let era = yoe.div_euclid(400);
        let yoe = yoe.rem_euclid(400);
        let mp = if m > 2 { m - 3 } else { m + 9 };
        let doy = (153 * mp + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146097 + doe - 719468;
        (days as u64) * 86400 + h * 3600 + min * 60 + s
    }
}
