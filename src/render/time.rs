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

fn zoned(then: i64, offset: i32) -> Option<jiff::Zoned> {
    let offset = jiff::tz::Offset::from_seconds(offset).unwrap_or(jiff::tz::Offset::UTC);
    let ts = jiff::Timestamp::from_second(then).ok()?;
    Some(ts.to_zoned(jiff::tz::TimeZone::fixed(offset)))
}

/// `Thu, 5 Nov 2026 14:03:00 +0100` in the recorded offset (patch headers).
pub fn rfc2822(then: i64, offset: i32) -> String {
    zoned(then, offset)
        .map(|z| z.strftime("%a, %-d %b %Y %H:%M:%S %z").to_string())
        .unwrap_or_default()
}

/// `2026-11-05 14:03 +0100` in the recorded offset.
pub fn local(then: i64, offset: i32) -> String {
    zoned(then, offset)
        .map(|z| z.strftime("%Y-%m-%d %H:%M %z").to_string())
        .unwrap_or_default()
}

/// `Nov 5, 2026` (UTC), for grouping commits by day.
pub fn day(then: i64) -> String {
    jiff::Timestamp::from_second(then)
        .map(|t| t.strftime("%b %-d, %Y").to_string())
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
        assert_eq!(rfc2822(0, 3600), "Thu, 1 Jan 1970 01:00:00 +0100");
        assert_eq!(local(0, -7200), "1969-12-31 22:00 -0200");
        assert_eq!(day(86400 * 5), "Jan 6, 1970");
    }
}
