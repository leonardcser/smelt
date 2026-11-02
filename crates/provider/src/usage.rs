//! Usage limits as managed providers report them, in a provider-neutral shape.

use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ManagedUsageRow {
    pub label: String,
    pub used: i64,
    pub limit: i64,
    #[serde(rename = "resetAt", skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<i64>,
}

#[derive(Debug, Default, Clone, Serialize, PartialEq, Eq)]
pub struct ManagedUsageReport {
    pub summary: Option<ManagedUsageRow>,
    pub limits: Vec<ManagedUsageRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credits: Option<String>,
    #[serde(rename = "resetCredits", skip_serializing_if = "Option::is_none")]
    pub reset_credits: Option<UsageResetCredits>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct UsageResetCredits {
    pub count: u64,
    pub inventory: Option<Vec<UsageResetCredit>>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct UsageResetCredit {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

pub fn parse_iso_time(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .or_else(|_| chrono::DateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f%z"))
        .ok()
        .map(|time| time.timestamp())
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
    fn parse_iso_time_rejects_invalid_dates_and_malformed_text() {
        for value in [
            "",
            "2026-02-30T00:00:00Z",
            "2026-01-01T25:00:00Z",
            "2026-01-01T00:00:00+99:99",
            "not a date",
            "🙂",
        ] {
            assert_eq!(parse_iso_time(value), None, "{value}");
        }
    }
}
