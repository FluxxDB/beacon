//! Time formatting: 12-hour clock for humans, ISO-8601 for JSON.

use jiff::{tz::TimeZone, Timestamp, Zoned};

/// Converts epoch milliseconds to a zoned time. Values outside the range jiff
/// supports (a corrupt event from a peer) clamp to its limits rather than panic.
fn zoned(ts_ms: i64, tz: &TimeZone) -> Zoned {
    let ts = Timestamp::from_millisecond(ts_ms).unwrap_or(if ts_ms < 0 {
        Timestamp::MIN
    } else {
        Timestamp::MAX
    });
    ts.to_zoned(tz.clone())
}

/// `6:07 pm`: 12-hour, lowercase am/pm, no leading zero on the hour.
pub fn clock_12h(ts_ms: i64, tz: &TimeZone) -> String {
    let t = zoned(ts_ms, tz);
    let hour = match t.hour() % 12 {
        0 => 12,
        h => h,
    };
    let suffix = if t.hour() < 12 { "am" } else { "pm" };
    format!("{hour}:{:02} {suffix}", t.minute())
}

/// `Today`, `Yesterday`, or a date like `Tue Oct 20`, judged by calendar day in `tz`.
pub fn day_label(ts_ms: i64, now_ms: i64, tz: &TimeZone) -> String {
    const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let date = zoned(ts_ms, tz).date();
    let today = zoned(now_ms, tz).date();
    if date == today {
        "Today".to_string()
    } else if today.yesterday().is_ok_and(|y| y == date) {
        "Yesterday".to_string()
    } else {
        format!(
            "{} {} {}",
            DAYS[date.weekday().to_monday_zero_offset() as usize],
            MONTHS[date.month() as usize - 1],
            date.day()
        )
    }
}

/// A compact age: `now` under a minute (or negative), then `1m`, `14m`, `3h`, `2d`.
pub fn age_short(delta_ms: i64) -> String {
    const MINUTE: i64 = 60_000;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    match delta_ms {
        d if d < MINUTE => "now".to_string(),
        d if d < HOUR => format!("{}m", d / MINUTE),
        d if d < DAY => format!("{}h", d / HOUR),
        d => format!("{}d", d / DAY),
    }
}

/// `2026-10-21T22:07:00.000Z`: UTC with millisecond precision.
pub fn iso8601(ts_ms: i64) -> String {
    let t = zoned(ts_ms, &TimeZone::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        t.year(),
        t.month(),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.subsec_nanosecond() / 1_000_000
    )
}
