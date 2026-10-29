//! The plan usage limits Claude Code reports for its account, as `/usage`
//! shows them.

use super::transport::Process;
use serde_json::{json, Value};
use smelt_provider::usage::{format_reset_time, ManagedUsageReport, ManagedUsageRow};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Asks the installed Claude Code for its account's usage limits. It starts
/// Claude Code without sending a model request.
pub async fn usage() -> Result<ManagedUsageReport, String> {
    let mut process = Process::spawn_bare("default")?;
    let usage = tokio::time::timeout(Duration::from_secs(30), async {
        process
            .ask("smelt-init", json!({"subtype":"initialize"}))
            .await?;
        process
            .ask(
                "smelt-usage",
                json!({"subtype":"get_usage", "skip_behaviors":true}),
            )
            .await
    })
    .await;
    process.close().await;
    let usage = usage.map_err(|_| "usage was not reported in time".to_string())??;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    parse(&usage, now)
}

fn parse(usage: &Value, now: u64) -> Result<ManagedUsageReport, String> {
    let limits = &usage["rate_limits"];
    if usage["rate_limits_available"] != true || !limits.is_object() {
        return Err("no plan usage limits for this account".into());
    }
    let row = |label: String, percent: &Value, resets_at: &Value| {
        Some(ManagedUsageRow {
            label,
            used: percent.as_f64()?.round() as i64,
            limit: 100,
            reset_hint: resets_at.as_str().map(|at| format_reset_time(at, now)),
        })
    };
    let mut rows: Vec<_> = match limits["limits"].as_array() {
        Some(listed) => listed
            .iter()
            .filter_map(|limit| {
                let label = match limit["group"].as_str() {
                    Some("session") => "5h limit".to_owned(),
                    Some("weekly") => "7d limit".to_owned(),
                    _ => limit["kind"].as_str()?.replace('_', " "),
                };
                let label = match limit["scope"]["model"]["display_name"].as_str() {
                    Some(model) => format!("{model} {label}"),
                    None => label,
                };
                row(label, &limit["percent"], &limit["resets_at"])
            })
            .collect(),
        // Releases before the `limits` list report the two plan windows.
        None => [("five_hour", "5h limit"), ("seven_day", "7d limit")]
            .into_iter()
            .filter_map(|(key, label)| {
                let window = &limits[key];
                row(label.into(), &window["utilization"], &window["resets_at"])
            })
            .collect(),
    };
    let spend = &limits["spend"];
    if spend["enabled"] == true {
        rows.extend(row("extra usage".into(), &spend["percent"], &Value::Null));
    }
    Ok(ManagedUsageReport {
        summary: None,
        limits: rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_793_181_600; // 2026-10-28T10:00:00Z

    fn labels(report: &ManagedUsageReport) -> Vec<(&str, i64, Option<&str>)> {
        report
            .limits
            .iter()
            .map(|row| (row.label.as_str(), row.used, row.reset_hint.as_deref()))
            .collect()
    }

    #[test]
    fn plan_limits_are_listed_with_their_scope_and_reset() {
        let report = parse(
            &json!({"rate_limits_available": true, "rate_limits": {
                "five_hour": {"utilization": 9, "resets_at": "2026-10-28T11:20:00.695098+00:00"},
                "limits": [
                    {"kind": "session", "group": "session", "percent": 9,
                     "resets_at": "2026-10-28T11:20:00.695098+00:00", "scope": null},
                    {"kind": "weekly_all", "group": "weekly", "percent": 20.4,
                     "resets_at": "2026-11-03T03:00:00+00:00", "scope": null},
                    {"kind": "weekly_scoped", "group": "weekly", "percent": 0,
                     "resets_at": null, "scope": {"model": {"id": null, "display_name": "Fable"}}},
                    {"kind": "monthly_cap", "group": "other", "percent": null},
                ],
                "spend": {"enabled": true, "percent": 35},
            }}),
            NOW,
        )
        .unwrap();
        assert_eq!(
            labels(&report),
            [
                ("5h limit", 9, Some("resets in 1h 20m")),
                ("7d limit", 20, Some("resets in 5d 17h")),
                ("Fable 7d limit", 0, None),
                ("extra usage", 35, None),
            ]
        );
        assert!(report.limits.iter().all(|row| row.limit == 100));
    }

    #[test]
    fn releases_without_the_limits_list_report_the_plan_windows() {
        let report = parse(
            &json!({"rate_limits_available": true, "rate_limits": {
                "five_hour": {"utilization": 54, "resets_at": "2026-10-28T11:00:00Z"},
                "seven_day": {"utilization": 18.4, "resets_at": null},
                "spend": {"enabled": false, "percent": 0},
            }}),
            NOW,
        )
        .unwrap();
        assert_eq!(
            labels(&report),
            [
                ("5h limit", 54, Some("resets in 1h")),
                ("7d limit", 18, None)
            ]
        );
    }

    #[test]
    fn accounts_without_plan_limits_say_so() {
        let error = parse(
            &json!({"rate_limits_available": false, "rate_limits": null}),
            NOW,
        )
        .unwrap_err();
        assert_eq!(error, "no plan usage limits for this account");
    }
}
