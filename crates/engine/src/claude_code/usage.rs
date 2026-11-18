//! The plan usage limits Claude Code reports for its account, as `/usage`
//! shows them, and the reset credits that clear them.

use super::account;
use super::transport::{self, Process};
use serde_json::{json, Value};
use smelt_provider::usage::{parse_iso_time, ManagedUsageReport, ManagedUsageRow};
use std::time::Duration;

/// Asks the installed Claude Code for its account's usage report without
/// sending a model request. Reporting usage refreshes its login.
async fn probe() -> Result<Value, String> {
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
    usage.map_err(|_| "usage was not reported in time".to_string())?
}

/// The account's usage limits, as Claude Code reports them, and the reset
/// credits it banks.
pub async fn usage(client: &reqwest::Client) -> Result<ManagedUsageReport, String> {
    let usage = probe().await?;
    let mut report = parse(&usage)?;
    report.reset_credits = account::reset_credits(client, scoped_model(&usage)).await;
    Ok(report)
}

/// The model with its own weekly limit, whose allowance model resets refill.
fn scoped_model(usage: &Value) -> &str {
    let limits = &usage["rate_limits"];
    limits["model_scoped"]
        .as_array()
        .and_then(|models| {
            models
                .iter()
                .find_map(|model| model["display_name"].as_str())
        })
        .or_else(|| {
            limits["limits"]
                .as_array()?
                .iter()
                .find_map(|limit| limit["scope"]["model"]["display_name"].as_str())
        })
        .unwrap_or("model")
}

/// Redeems one reset of grant `credit_id` for the account `scope` names.
/// Reuse `request_id` to retry an uncertain outcome without spending twice.
pub async fn redeem_reset(
    client: &reqwest::Client,
    scope: &str,
    credit_id: &str,
    request_id: &str,
) -> Result<String, String> {
    let login = match account::login() {
        Ok(login) if login.lasts() => login,
        // Claude Code refreshes its login while it reports usage.
        _ => {
            probe().await?;
            account::login()?
        }
    };
    let version = transport::version().await?;
    if account::usage_scope().as_deref() != Some(scope) {
        return Err("Account changed - reopen usage before redeeming a reset.".into());
    }
    let claimed = account::claim(
        client,
        account::API_BASE,
        &login,
        &version,
        credit_id,
        request_id,
    )
    .await;
    account::forget_credits();
    claimed
}

fn parse(usage: &Value) -> Result<ManagedUsageReport, String> {
    let limits = &usage["rate_limits"];
    if usage["rate_limits_available"] != true || !limits.is_object() {
        return Err("no plan usage limits for this account".into());
    }
    let row = |label: String, percent: &Value, resets_at: &Value| {
        Some(ManagedUsageRow {
            label,
            used: percent.as_f64()?.round() as i64,
            limit: 100,
            reset_at: resets_at.as_str().and_then(parse_iso_time),
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
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(report: &ManagedUsageReport) -> Vec<(&str, i64, Option<i64>)> {
        report
            .limits
            .iter()
            .map(|row| (row.label.as_str(), row.used, row.reset_at))
            .collect()
    }

    #[test]
    fn plan_limits_are_listed_with_their_scope_and_reset() {
        let report = parse(&json!({"rate_limits_available": true, "rate_limits": {
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
        }}))
        .unwrap();
        assert_eq!(
            labels(&report),
            [
                ("5h limit", 9, parse_iso_time("2026-10-28T11:20:00Z")),
                ("7d limit", 20, parse_iso_time("2026-11-03T03:00:00Z")),
                ("Fable 7d limit", 0, None),
                ("extra usage", 35, None),
            ]
        );
        assert!(report.limits.iter().all(|row| row.limit == 100));
    }

    #[test]
    fn releases_without_the_limits_list_report_the_plan_windows() {
        let report = parse(&json!({"rate_limits_available": true, "rate_limits": {
            "five_hour": {"utilization": 54, "resets_at": "2026-10-28T11:00:00Z"},
            "seven_day": {"utilization": 18.4, "resets_at": null},
            "spend": {"enabled": false, "percent": 0},
        }}))
        .unwrap();
        assert_eq!(
            labels(&report),
            [
                ("5h limit", 54, parse_iso_time("2026-10-28T11:00:00Z")),
                ("7d limit", 18, None)
            ]
        );
    }

    #[test]
    fn model_resets_name_the_model_with_its_own_weekly_limit() {
        let scoped = |rate_limits: Value| json!({"rate_limits": rate_limits});
        assert_eq!(
            scoped_model(&scoped(
                json!({"model_scoped": [{"display_name": "Fable"}]})
            )),
            "Fable"
        );
        assert_eq!(
            scoped_model(&scoped(json!({"limits": [
                {"scope": null},
                {"scope": {"model": {"display_name": "Opus"}}},
            ]}))),
            "Opus"
        );
        assert_eq!(scoped_model(&scoped(json!({}))), "model");
    }

    #[test]
    fn accounts_without_plan_limits_say_so() {
        let error =
            parse(&json!({"rate_limits_available": false, "rate_limits": null})).unwrap_err();
        assert_eq!(error, "no plan usage limits for this account");
    }
}
