//! UTC timestamps, without a dependency.
//!
//! Kiln needs exactly two things from a clock: an RFC 3339 stamp for the build
//! record, and today's date for the snapshot a rolling build resolved on.
//! Neither is worth a date-time crate, and both must agree, so they
//! share one civil-date conversion.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch. `SOURCE_DATE_EPOCH` wins when set, so a
/// reproducibility harness can pin the record's timestamp the same way it pins
/// everything else.
pub fn now() -> i64 {
    if let Ok(s) = std::env::var("SOURCE_DATE_EPOCH") {
        if let Ok(n) = s.trim().parse::<i64>() {
            return n;
        }
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `2026-08-30T19:04:11Z`.
pub fn rfc3339(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// `2026-08-30` — the form `repos.snapshot` takes, so that a recorded snapshot
/// can be pasted straight into a configuration.
pub fn date(secs: i64) -> String {
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since 1970-01-01 → (year, month, day). Howard Hinnant's `civil_from_days`,
/// which is exact for the whole proleptic Gregorian range and has no branches
/// worth arguing about.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
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
    fn known_instants_round_trip() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_000_000_000), "2001-09-09T01:46:40Z");
        // A leap day, because the month arithmetic is where this goes wrong.
        assert_eq!(date(1_709_164_800), "2024-02-29");
        assert_eq!(date(1_798_675_200), "2026-12-31");
        assert_eq!(date(1_798_761_600), "2027-01-01");
    }

    #[test]
    fn the_date_is_the_prefix_of_the_stamp() {
        for secs in [0, 1, 86_399, 86_400, 1_756_000_000] {
            assert!(rfc3339(secs).starts_with(&date(secs)));
        }
    }
}
