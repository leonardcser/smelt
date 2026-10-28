//! Usage limits as managed providers report them, in a provider-neutral shape.

use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ManagedUsageRow {
    pub label: String,
    pub used: i64,
    pub limit: i64,
    #[serde(rename = "resetHint", skip_serializing_if = "Option::is_none")]
    pub reset_hint: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ManagedUsageReport {
    pub summary: Option<ManagedUsageRow>,
    pub limits: Vec<ManagedUsageRow>,
}

/// `resets in 2h 10m` for an ISO 8601 reset time, measured from `now` in
/// Unix seconds.
pub fn format_reset_time(value: &str, now: u64) -> String {
    let Some(stamp) = parse_iso_time(value) else {
        return format!("resets at {value}");
    };
    let diff = stamp - now as i64;
    if diff <= 0 {
        return "reset".to_string();
    }
    format!("resets in {}", format_duration(diff))
}

pub fn parse_iso_time(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() < 19
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || bytes.get(10) != Some(&b'T')
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return None;
    }
    let y = value.get(0..4)?.parse::<i32>().ok()?;
    let mo = value.get(5..7)?.parse::<u32>().ok()?;
    let d = value.get(8..10)?.parse::<u32>().ok()?;
    let h = value.get(11..13)?.parse::<i64>().ok()?;
    let mi = value.get(14..16)?.parse::<i64>().ok()?;
    let s = value.get(17..19)?.parse::<i64>().ok()?;
    let rest = &value[19..];
    let tz = rest
        .find(['Z', 'z', '+', '-'])
        .and_then(|idx| rest.get(idx..))
        .unwrap_or("Z");
    let offset = parse_timezone_offset(tz)?;
    Some(days_from_civil(y, mo, d)? * 86_400 + h * 3600 + mi * 60 + s - offset)
}

fn parse_timezone_offset(value: &str) -> Option<i64> {
    if value.starts_with('Z') || value.starts_with('z') {
        return Some(0);
    }
    let sign = match value.as_bytes().first().copied()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digits: String = value[1..]
        .chars()
        .filter(|c| c.is_ascii_digit())
        .take(4)
        .collect();
    if digits.len() != 4 {
        return None;
    }
    let hours = digits[0..2].parse::<i64>().ok()?;
    let minutes = digits[2..4].parse::<i64>().ok()?;
    Some(sign * (hours * 3600 + minutes * 60))
}

fn days_from_civil(year: i32, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let y = year - i32::from(month <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = month as i32 + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day as i32 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146097 + doe - 719468) as i64)
}

pub fn format_duration(total_seconds: i64) -> String {
    if total_seconds <= 0 {
        return "0s".to_string();
    }
    let days = total_seconds / 86_400;
    let hours = (total_seconds % 86_400) / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    let mut parts = Vec::new();
    if days > 0 {
        parts.push(format!("{days}d"));
    }
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if minutes > 0 {
        parts.push(format!("{minutes}m"));
    }
    if seconds > 0 && parts.is_empty() {
        parts.push(format!("{seconds}s"));
    }
    if parts.is_empty() {
        "0s".to_string()
    } else {
        parts.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_iso_time_honors_timezone_offsets_and_fractional_seconds() {
        assert_eq!(parse_iso_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso_time("1970-01-01T02:00:00.123+02:00"), Some(0));
        assert_eq!(parse_iso_time("1969-12-31T19:00:00-0500"), Some(0));
    }

    #[test]
    fn format_duration_names_the_two_largest_units() {
        assert_eq!(format_duration(45), "45s");
        assert_eq!(format_duration(90), "1m");
        assert_eq!(format_duration(3661), "1h 1m");
        assert_eq!(format_duration(86_400 + 7200 + 600), "1d 2h 10m");
    }
}
