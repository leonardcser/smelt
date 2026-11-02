//! Account usage read through smelt-managed authentication.

use super::AuthenticatedResponse;
use serde_json::Value;
use smelt_provider::codex::CodexTokens;
use smelt_provider::usage::{
    parse_iso_time, ManagedUsageReport, ManagedUsageRow, UsageResetCredit, UsageResetCredits,
};
use std::time::{SystemTime, UNIX_EPOCH};

async fn codex_json(
    client: &reqwest::Client,
    tokens: &CodexTokens,
    path: &str,
) -> Result<Value, String> {
    let url = format!("{}{path}", smelt_provider::codex::CHATGPT_BACKEND_API_BASE);
    let response = super::send_authenticated_request(client, "GET", url, None, |request| {
        tokens.apply_headers(request)
    })
    .await?;
    parse_response(&response)
}

fn parse_response(response: &AuthenticatedResponse) -> Result<Value, String> {
    match response.status {
        200 => serde_json::from_str::<Value>(&response.body)
            .ok()
            .filter(Value::is_object)
            .ok_or_else(|| "usage response was invalid - try again later".into()),
        401 => Err("authentication expired - run `smelt auth` to sign in again".into()),
        403 => Err("usage unavailable for this account".into()),
        _ => Err("usage unavailable right now - try again later".into()),
    }
}

pub(super) async fn codex(client: &reqwest::Client) -> Result<ManagedUsageReport, String> {
    let tokens = crate::provider::codex::ensure_access_token_full(client).await?;
    let payload = codex_json(client, &tokens, "/wham/usage").await?;
    let mut report = parse_codex(&payload)?;
    if let Some(credits) = &mut report.reset_credits {
        if credits.count > 0 {
            // Inventory is supplementary: its failure must not hide plan usage.
            if let Ok(inventory) =
                codex_json(client, &tokens, "/wham/rate-limit-reset-credits").await
            {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;
                credits.inventory = parse_inventory(&inventory, credits.count, now);
            }
        }
    }
    Ok(report)
}

pub(super) async fn redeem(
    tokens: &CodexTokens,
    scope: &str,
    request_id: &str,
    client: &reqwest::Client,
    url: &str,
) -> Result<String, String> {
    if super::codex_account_scope(tokens) != scope {
        return Err("Account changed - reopen usage before redeeming a reset.".into());
    }
    if request_id.is_empty()
        || request_id.len() > 128
        || !request_id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err("invalid reset request id".into());
    }
    let body = serde_json::to_vec(&serde_json::json!({ "redeem_request_id": request_id }))
        .map_err(|error| error.to_string())?;
    let response =
        super::send_authenticated_request(client, "POST", url.into(), Some(body), |request| {
            tokens.apply_headers(request)
        })
        .await?;
    redemption_message(&parse_response(&response)?)
}

fn redemption_message(payload: &Value) -> Result<String, String> {
    let outcome = payload["code"]
        .as_str()
        .or_else(|| payload["outcome"].as_str())
        .or_else(|| payload["result"].as_str())
        .unwrap_or_default()
        .replace('_', "")
        .to_lowercase();
    match outcome.as_str() {
        "reset" => match payload["windows_reset"].as_u64().filter(|count| *count > 0) {
            Some(count) => Ok(format!(
                "Redeemed reset credit. Reset {count} usage window{}.",
                if count == 1 { "" } else { "s" }
            )),
            None => Ok("Redeemed reset credit.".into()),
        },
        "nothingtoreset" => Ok("No current usage limit window can be reset.".into()),
        "alreadyredeemed" => Ok("This reset request was already redeemed.".into()),
        "nocredit" => Err("No usage limit reset credits are available.".into()),
        _ => Err("Unexpected reset response.".into()),
    }
}

fn invalid_usage() -> String {
    "usage response was invalid - try again later".into()
}

fn parse_codex(payload: &Value) -> Result<ManagedUsageReport, String> {
    if !payload["rate_limit"].is_object()
        && !payload["additional_rate_limits"].is_array()
        && !payload["plan_type"].is_string()
        && !payload["credits"].is_object()
    {
        return Err(invalid_usage());
    }
    let mut report = ManagedUsageReport {
        plan: payload["plan_type"].as_str().map(str::to_owned),
        ..Default::default()
    };
    let credits = &payload["credits"];
    if credits["has_credits"] == true {
        report.credits = if credits["unlimited"] == true {
            Some("unlimited".into())
        } else {
            match &credits["balance"] {
                Value::String(value) => Some(value.clone()),
                Value::Number(value) => Some(value.to_string()),
                _ => None,
            }
        };
    }
    let count = &payload["rate_limit_reset_credits"]["available_count"];
    if !count.is_null() {
        report.reset_credits = Some(UsageResetCredits {
            count: count.as_u64().ok_or_else(invalid_usage)?,
            inventory: None,
        });
    }
    add_windows(&mut report.limits, &payload["rate_limit"], "")?;
    if let Some(extras) = payload["additional_rate_limits"].as_array() {
        for extra in extras {
            let name = extra["limit_name"]
                .as_str()
                .or_else(|| extra["metered_feature"].as_str())
                .unwrap_or("additional");
            add_windows(
                &mut report.limits,
                &extra["rate_limit"],
                &format!("{name} "),
            )?;
        }
    }
    Ok(report)
}

fn add_windows(
    rows: &mut Vec<ManagedUsageRow>,
    limits: &Value,
    prefix: &str,
) -> Result<(), String> {
    if !limits.is_null() && !limits.is_object() {
        return Err(invalid_usage());
    }
    for (key, fallback) in [
        ("primary_window", "primary"),
        ("secondary_window", "secondary"),
    ] {
        let window = &limits[key];
        if window.is_null() {
            continue;
        }
        if !window.is_object() {
            return Err(invalid_usage());
        }
        let percent = &window["used_percent"];
        let used = percent
            .as_f64()
            .or_else(|| percent.as_str()?.parse::<f64>().ok())
            .filter(|value| value.is_finite() && *value >= 0.0)
            .ok_or_else(invalid_usage)?;
        let label = match window["limit_window_seconds"].as_u64().map(|n| n / 60) {
            None => fallback.to_owned(),
            Some(minutes) if (28 * 1440..=31 * 1440).contains(&minutes) => "monthly limit".into(),
            Some(minutes) if minutes >= 1440 && minutes % 1440 == 0 => {
                format!("{}d limit", minutes / 1440)
            }
            Some(minutes) if minutes >= 60 && minutes % 60 == 0 => {
                format!("{}h limit", minutes / 60)
            }
            Some(minutes) => format!("{minutes}m limit"),
        };
        rows.push(ManagedUsageRow {
            label: format!("{prefix}{label}"),
            used: used.round() as i64,
            limit: 100,
            reset_at: window["reset_at"].as_i64(),
        });
    }
    Ok(())
}

fn parse_inventory(payload: &Value, count: u64, now: i64) -> Option<Vec<UsageResetCredit>> {
    let mut inventory = Vec::new();
    for credit in payload["credits"]
        .as_array()?
        .iter()
        .filter(|credit| credit["status"] == "available")
    {
        let expires_at = match &credit["expires_at"] {
            Value::Null => None,
            Value::Number(value) => Some(value.as_i64()?),
            Value::String(value) => Some(parse_iso_time(value)?),
            _ => return None,
        };
        if expires_at.is_some_and(|at| at <= now) {
            continue;
        }
        inventory.push(UsageResetCredit { expires_at });
    }
    (inventory.len() as u64 == count).then_some(inventory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn codex_errors_do_not_cache_or_expose_response_bodies() {
        for (status, body, expected) in [
            (401, "private account data", "authentication expired"),
            (403, "private account data", "unavailable for this account"),
            (500, "private account data", "unavailable right now"),
            (200, "not json", "response was invalid"),
            (200, "[]", "response was invalid"),
        ] {
            let err = parse_response(&AuthenticatedResponse {
                status,
                body: body.into(),
            })
            .unwrap_err();
            assert!(err.contains(expected), "{err}");
            assert!(!err.contains(body));
        }
        assert!(parse_response(&AuthenticatedResponse {
            status: 200,
            body: "{}".into()
        })
        .is_ok());
    }

    #[test]
    fn codex_report_keeps_plan_credits_windows_and_reset_state() {
        let report = parse_codex(&json!({
            "plan_type": "plus", "credits": { "has_credits": true, "balance": 12 },
            "rate_limit_reset_credits": { "available_count": 2 },
            "rate_limit": { "primary_window": { "used_percent": 82.4,
                "limit_window_seconds": 18000, "reset_at": 1893456000 } },
            "additional_rate_limits": [{ "limit_name": "review", "rate_limit": {
                "secondary_window": { "used_percent": 105, "limit_window_seconds": 604800 }
            }}]
        }))
        .unwrap();
        assert_eq!(report.plan.as_deref(), Some("plus"));
        assert_eq!(report.credits.as_deref(), Some("12"));
        assert_eq!(report.reset_credits.unwrap().count, 2);
        assert_eq!(report.limits[0].label, "5h limit");
        assert_eq!(report.limits[0].used, 82);
        assert_eq!(report.limits[0].reset_at, Some(1893456000));
        assert_eq!(report.limits[1].label, "review 7d limit");
        assert_eq!(report.limits[1].used, 105);
    }

    #[test]
    fn malformed_reports_never_invent_zero_usage_or_nonexpiring_credits() {
        for payload in [
            json!({}),
            json!({"error": "unavailable"}),
            json!({"rate_limit": {"primary_window": {}}}),
            json!({"rate_limit": {"primary_window": {"used_percent": "unknown"}}}),
            json!({"rate_limit": {"primary_window": {"used_percent": -1}}}),
        ] {
            assert!(parse_codex(&payload).is_err(), "{payload}");
        }
        let report =
            parse_codex(&json!({"rate_limit": {"primary_window": {"used_percent": "42.5"}}}))
                .unwrap();
        assert_eq!(report.limits[0].used, 43);
        assert!(parse_inventory(
            &json!({"credits": [{"status": "available", "expires_at": "invalid"}]}),
            1,
            0
        )
        .is_none());
    }

    #[tokio::test]
    async fn redemption_uses_confirmed_credentials_and_idempotency_key() {
        let (host, request) = crate::provider::test_http::spawn_json_response(
            r#"{"code":"reset","windows_reset":2}"#,
        )
        .await;
        let tokens = smelt_provider::codex::CodexTokens {
            access_token: "fixture-access".into(),
            refresh_token: "fixture-refresh".into(),
            account_id: Some("confirmed-account".into()),
            expires_at: u64::MAX,
            last_refresh: 0,
        };
        let scope = super::super::codex_account_scope(&tokens);
        let client = smelt_provider::http_client();
        let url = format!("{host}/wham/rate-limit-reset-credits/consume");
        assert!(
            redeem(&tokens, "different-account", "operation-1", &client, &url)
                .await
                .unwrap_err()
                .contains("Account changed")
        );
        assert!(redeem(&tokens, &scope, "operation-1", &client, &url)
            .await
            .unwrap()
            .contains("Reset 2 usage windows"));
        let request = request.await.unwrap();
        assert!(request.starts_with("POST /wham/rate-limit-reset-credits/consume"));
        assert!(request.contains("confirmed-account") && request.contains("operation-1"));
    }

    #[test]
    fn redemption_outcomes_are_explicit() {
        assert!(redemption_message(&json!({"outcome":"already_redeemed"})).is_ok());
        assert!(redemption_message(&json!({"code":"nothing_to_reset"})).is_ok());
        assert!(redemption_message(&json!({"code":"no_credit"})).is_err());
        assert!(redemption_message(&json!({"code":"unknown"})).is_err());
    }

    #[test]
    fn inventory_ignores_spent_and_expired_credits_and_requires_matching_count() {
        let payload = json!({"credits": [
            {"status": "available", "expires_at": "2030-01-01T00:00:00Z"},
            {"status": "available", "expires_at": null},
            {"status": "available", "expires_at": 1},
            {"status": "spent", "expires_at": null}
        ]});
        let inventory = parse_inventory(&payload, 2, 100).unwrap();
        assert_eq!(inventory[0].expires_at, Some(1893456000));
        assert_eq!(inventory[1].expires_at, None);
        assert!(parse_inventory(&payload, 3, 100).is_none());
    }
}
