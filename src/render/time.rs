//! Human-readable times.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// "3 hours ago", relative to `now` (both unix seconds). Future times
/// (clock skew) read as "just now".
pub fn relative(then: i64, now: i64) -> String {
    const MIN: i64 = 60;
    const HOUR: i64 = 60 * MIN;
    const DAY: i64 = 24 * HOUR;
    const WEEK: i64 = 7 * DAY;
    const MONTH: i64 = 30 * DAY;
    const YEAR: i64 = 365 * DAY;

    let delta = now - then;
    let (n, unit) = match delta {
        d if d < MIN => return "just now".into(),
        d if d < HOUR => (d / MIN, "minute"),
        d if d < DAY => (d / HOUR, "hour"),
        d if d < 2 * WEEK => (d / DAY, "day"),
        d if d < 2 * MONTH => (d / WEEK, "week"),
        d if d < YEAR => (d / MONTH, "month"),
        d => (d / YEAR, "year"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

/// RFC 3339 UTC timestamp, for `<time datetime>` and tooltips.
pub fn iso(then: i64) -> String {
    jiff::Timestamp::from_second(then)
        .map(|t| t.strftime("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_times() {
        let now = 1_000_000_000;
        assert_eq!(relative(now + 5, now), "just now");
        assert_eq!(relative(now - 59, now), "just now");
        assert_eq!(relative(now - 60, now), "1 minute ago");
        assert_eq!(relative(now - 2 * 3600, now), "2 hours ago");
        assert_eq!(relative(now - 86400, now), "1 day ago");
        assert_eq!(relative(now - 13 * 86400, now), "13 days ago");
        assert_eq!(relative(now - 15 * 86400, now), "2 weeks ago");
        assert_eq!(relative(now - 90 * 86400, now), "3 months ago");
        assert_eq!(relative(now - 800 * 86400, now), "2 years ago");
    }

    #[test]
    fn iso_format() {
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
    }
}
