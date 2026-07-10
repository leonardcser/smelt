//! Kimi Code provider protocol helpers.
//!
//! This module owns Kimi OAuth/device-code HTTP protocol, authenticated API
//! request helpers, and provider response parsing. Credential and model-cache
//! persistence stay outside this crate.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

pub const API_BASE: &str = "https://api.kimi.com/coding/v1";
pub const OAUTH_HOST: &str = "https://auth.kimi.com";
const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KimiCodeModelInfo {
    pub id: String,
    pub context_length: Option<u32>,
    pub supports_reasoning: Option<bool>,
    pub supports_image_in: Option<bool>,
    pub supports_video_in: Option<bool>,
    pub supports_tool_use: Option<bool>,
    pub display_name: Option<String>,
}

impl KimiCodeModelInfo {
    pub fn from_json(item: &serde_json::Value) -> Option<Self> {
        let id = item["id"].as_str()?.to_string();
        let display_name = item["display_name"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        Some(Self {
            id,
            context_length: json_u32(&item["context_length"]),
            supports_reasoning: item.get("supports_reasoning").and_then(|v| v.as_bool()),
            supports_image_in: item.get("supports_image_in").and_then(|v| v.as_bool()),
            supports_video_in: item.get("supports_video_in").and_then(|v| v.as_bool()),
            supports_tool_use: item.get("supports_tool_use").and_then(|v| v.as_bool()),
            display_name,
        })
    }

    pub fn matches_name(&self, model: &str) -> bool {
        self.id.eq_ignore_ascii_case(model)
            || self
                .display_name
                .as_deref()
                .is_some_and(|name| name.eq_ignore_ascii_case(model))
    }
}

impl From<KimiCodeModelInfo> for protocol::ModelMetadata {
    fn from(model: KimiCodeModelInfo) -> Self {
        let mut input_modalities = vec!["text".to_string()];
        if model.supports_image_in == Some(true) {
            input_modalities.push("image".to_string());
        }
        if model.supports_video_in == Some(true) {
            input_modalities.push("video".to_string());
        }
        Self {
            id: model.id,
            display_name: model.display_name,
            context_window: model.context_length,
            max_output_tokens: None,
            supports_reasoning: model.supports_reasoning,
            supports_fast_mode: None,
            input_modalities: Some(input_modalities),
        }
    }
}

pub fn json_u32(value: &serde_json::Value) -> Option<u32> {
    value
        .as_u64()
        .or_else(|| value.as_str()?.parse::<u64>().ok())
        .and_then(|v| u32::try_from(v).ok())
        .filter(|v| *v > 0)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KimiCodeTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
    #[serde(default)]
    pub scope: String,
    #[serde(default = "default_bearer")]
    pub token_type: String,
}

pub fn default_bearer() -> String {
    "Bearer".to_string()
}

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

pub fn parse_managed_usage_payload(payload: &Value, now: u64) -> ManagedUsageReport {
    let Some(rec) = payload.as_object() else {
        return ManagedUsageReport {
            summary: None,
            limits: Vec::new(),
        };
    };

    let summary = to_usage_row(rec.get("usage"), "Weekly limit", now);
    let mut limits = Vec::new();
    if let Some(items) = rec.get("limits").and_then(Value::as_array) {
        for (idx, item) in items.iter().enumerate() {
            let Some(item_obj) = item.as_object() else {
                continue;
            };
            let detail = item_obj
                .get("detail")
                .and_then(Value::as_object)
                .unwrap_or(item_obj);
            let empty = serde_json::Map::new();
            let window = item_obj
                .get("window")
                .and_then(Value::as_object)
                .unwrap_or(&empty);
            let label = limit_label(item_obj, detail, window, idx);
            if let Some(row) = to_usage_row(Some(&Value::Object(detail.clone())), &label, now) {
                limits.push(row);
            }
        }
    }

    ManagedUsageReport { summary, limits }
}

fn to_usage_row(raw: Option<&Value>, default_label: &str, now: u64) -> Option<ManagedUsageRow> {
    let raw = raw?.as_object()?;
    let limit = to_int(raw.get("limit"));
    let mut used = to_int(raw.get("used"));
    if used.is_none() {
        if let (Some(remaining), Some(limit)) = (to_int(raw.get("remaining")), limit) {
            used = Some(limit - remaining);
        }
    }
    if used.is_none() && limit.is_none() {
        return None;
    }
    let label = string_field(raw, "name")
        .or_else(|| string_field(raw, "title"))
        .unwrap_or_else(|| default_label.to_string());
    Some(ManagedUsageRow {
        label,
        used: used.unwrap_or(0),
        limit: limit.unwrap_or(0),
        reset_hint: reset_hint_from(raw, now),
    })
}

fn limit_label(
    item: &serde_json::Map<String, Value>,
    detail: &serde_json::Map<String, Value>,
    window: &serde_json::Map<String, Value>,
    idx: usize,
) -> String {
    for key in ["name", "title", "scope"] {
        if let Some(value) = string_field(item, key).or_else(|| string_field(detail, key)) {
            return value;
        }
    }
    let duration = to_int(
        window
            .get("duration")
            .or_else(|| item.get("duration"))
            .or_else(|| detail.get("duration")),
    );
    let time_unit = window
        .get("timeUnit")
        .or_else(|| item.get("timeUnit"))
        .or_else(|| detail.get("timeUnit"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if let Some(duration) = duration {
        if time_unit.contains("MINUTE") {
            if duration >= 60 && duration % 60 == 0 {
                return format!("{}h limit", duration / 60);
            }
            return format!("{duration}m limit");
        }
        if time_unit.contains("HOUR") {
            return format!("{duration}h limit");
        }
        if time_unit.contains("DAY") {
            return format!("{duration}d limit");
        }
        return format!("{duration}s limit");
    }
    format!("Limit #{}", idx + 1)
}

fn reset_hint_from(raw: &serde_json::Map<String, Value>, now: u64) -> Option<String> {
    for key in ["reset_at", "resetAt", "reset_time", "resetTime"] {
        if let Some(value) = raw
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            return Some(format_reset_time(value, now));
        }
    }
    for key in ["reset_in", "resetIn", "ttl", "window"] {
        if let Some(seconds) = to_int(raw.get(key)).filter(|s| *s > 0) {
            return Some(format!("resets in {}", format_duration(seconds)));
        }
    }
    None
}

fn format_reset_time(value: &str, now: u64) -> String {
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

fn to_int(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|n| n.trunc() as i64)),
        Value::String(s) => s.parse::<f64>().ok().map(|n| n.trunc() as i64),
        _ => None,
    }
}

fn string_field(map: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    map.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

pub fn api_error_message(body: &str) -> Option<String> {
    let payload: Value = serde_json::from_str(body).ok()?;
    payload
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| payload.pointer("/error/message").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[derive(Clone)]
pub struct KimiHeaders {
    pub user_agent: String,
    pub version: String,
    pub device_name: String,
    pub device_model: String,
    pub os_version: String,
    pub device_id: String,
}

pub fn apply_default_headers(
    mut req: reqwest::RequestBuilder,
    headers: &KimiHeaders,
) -> reqwest::RequestBuilder {
    req = req
        .header("User-Agent", &headers.user_agent)
        .header("X-Msh-Platform", "kimi_code_cli")
        .header("X-Msh-Version", &headers.version)
        .header("X-Msh-Device-Name", &headers.device_name)
        .header("X-Msh-Device-Model", &headers.device_model)
        .header("X-Msh-Os-Version", &headers.os_version)
        .header("X-Msh-Device-Id", &headers.device_id);
    req
}

#[derive(Clone)]
pub struct KimiAuthEnv {
    pub oauth_host: String,
    pub api_base: String,
    pub headers: KimiHeaders,
    pub now: fn() -> u64,
}

pub struct KimiLoginProgress<'a> {
    pub on_prompt: &'a (dyn Fn(&str, &str) + Send + Sync),
}

pub struct OAuthResponse {
    pub status: u16,
    pub body: String,
    pub json: serde_json::Value,
}

pub struct OAuthFormRequest {
    pub url: String,
    pub body: String,
}

pub fn build_oauth_form_request(
    env: &KimiAuthEnv,
    path: &str,
    params: &[(&str, &str)],
) -> OAuthFormRequest {
    OAuthFormRequest {
        url: format!("{}{}", env.oauth_host, path),
        body: url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(params.iter().copied())
            .finish(),
    }
}

pub async fn post_form(
    client: &reqwest::Client,
    env: &KimiAuthEnv,
    path: &str,
    params: &[(&str, &str)],
) -> Result<OAuthResponse, String> {
    let spec = build_oauth_form_request(env, path, params);
    let resp = apply_default_headers(client.post(&spec.url), &env.headers)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(spec.body)
        .send()
        .await
        .map_err(|e| format!("OAuth request failed: {e}"))?;
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    let json = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    Ok(OAuthResponse { status, body, json })
}

pub fn oauth_error(resp: &OAuthResponse, context: &str) -> String {
    let detail = resp.json["error_description"]
        .as_str()
        .or_else(|| resp.json["detail"].as_str())
        .or_else(|| resp.json["message"].as_str())
        .or_else(|| resp.json["error"].as_str())
        .unwrap_or(resp.body.as_str());
    format!("{context} (HTTP {}): {detail}", resp.status)
}

#[derive(Debug, PartialEq, Eq)]
pub enum DevicePollDecision {
    Authorized,
    Continue { interval: u64 },
    Expired,
    Denied,
}

pub fn device_poll_decision(
    resp: &OAuthResponse,
    interval: u64,
) -> Result<DevicePollDecision, String> {
    if resp.status == 200 {
        return Ok(DevicePollDecision::Authorized);
    }

    match resp.json["error"].as_str().unwrap_or_default() {
        "authorization_pending" => Ok(DevicePollDecision::Continue { interval }),
        "slow_down" => Ok(DevicePollDecision::Continue {
            interval: interval.saturating_add(5),
        }),
        "expired_token" => Ok(DevicePollDecision::Expired),
        "access_denied" => Ok(DevicePollDecision::Denied),
        _ => Err(oauth_error(resp, "Device token polling failed")),
    }
}

pub struct KimiLoginOutcome {
    pub tokens: KimiCodeTokens,
    pub models: Vec<KimiCodeModelInfo>,
}

pub async fn login(
    client: &reqwest::Client,
    progress: &KimiLoginProgress<'_>,
    env: &KimiAuthEnv,
) -> Result<KimiLoginOutcome, String> {
    let auth = post_form(
        client,
        env,
        "/api/oauth/device_authorization",
        &[("client_id", CLIENT_ID)],
    )
    .await?;
    if auth.status != 200 {
        return Err(oauth_error(&auth, "Device authorization failed"));
    }

    let user_code = auth.json["user_code"].as_str().unwrap_or_default();
    let device_code = auth.json["device_code"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("device authorization response missing device_code")?;
    let callback_url = auth.json["verification_uri_complete"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("device authorization response missing verification_uri_complete")?;
    let mut interval = auth.json["interval"].as_u64().unwrap_or(5);
    let expires_at =
        (env.now)().saturating_add(auth.json["expires_in"].as_u64().unwrap_or(15 * 60));
    (progress.on_prompt)(callback_url, user_code);

    while (env.now)() < expires_at {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let resp = post_form(
            client,
            env,
            "/api/oauth/token",
            &[
                ("client_id", CLIENT_ID),
                ("device_code", device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ],
        )
        .await?;

        match device_poll_decision(&resp, interval)? {
            DevicePollDecision::Authorized => {
                let tokens = token_from_response_at(&resp.json, (env.now)())?;
                let models = fetch_models_with_token(client, env, &tokens.access_token)
                    .await
                    .unwrap_or_default();
                return Ok(KimiLoginOutcome { tokens, models });
            }
            DevicePollDecision::Continue {
                interval: next_interval,
            } => interval = next_interval,
            DevicePollDecision::Expired => {
                return Err("Kimi Code authorization expired; run login again".to_string());
            }
            DevicePollDecision::Denied => {
                return Err("Kimi Code authorization was denied".to_string())
            }
        }
    }

    Err("Kimi Code authorization expired; run login again".to_string())
}

pub fn token_from_response_at(
    value: &serde_json::Value,
    now: u64,
) -> Result<KimiCodeTokens, String> {
    let access_token = value["access_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("OAuth response missing access_token")?
        .to_string();
    let refresh_token = value["refresh_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("OAuth response missing refresh_token")?
        .to_string();
    let expires_in = value["expires_in"]
        .as_u64()
        .ok_or("OAuth response missing expires_in")?;
    Ok(KimiCodeTokens {
        access_token,
        refresh_token,
        expires_at: now.saturating_add(expires_in),
        scope: value["scope"].as_str().unwrap_or_default().to_string(),
        token_type: value["token_type"].as_str().unwrap_or("Bearer").to_string(),
    })
}

pub async fn refresh_access_token(
    client: &reqwest::Client,
    env: &KimiAuthEnv,
    refresh_token: &str,
) -> Result<KimiCodeTokens, String> {
    let resp = post_form(
        client,
        env,
        "/api/oauth/token",
        &[
            ("client_id", CLIENT_ID),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ],
    )
    .await?;
    if resp.status != 200 {
        return Err(oauth_error(&resp, "Token refresh failed"));
    }
    token_from_response_at(&resp.json, (env.now)())
}

pub struct AuthenticatedResponse {
    pub status: u16,
    pub body: String,
}

pub async fn authenticated_request(
    client: &reqwest::Client,
    env: &KimiAuthEnv,
    method: &str,
    path: &str,
    body: Option<Vec<u8>>,
    access_token: &str,
) -> Result<AuthenticatedResponse, String> {
    let path = authenticated_path(path)?;
    let url = format!("{}{path}", env.api_base);
    let method = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|e| format!("invalid HTTP method: {e}"))?;
    let mut req = apply_default_headers(client.request(method, url), &env.headers)
        .bearer_auth(access_token)
        .header("Accept", "application/json");
    if let Some(body) = body {
        req = req.header("Content-Type", "application/json").body(body);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("authenticated request failed: {e}"))?;
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    Ok(AuthenticatedResponse { status, body })
}

pub fn authenticated_path(path: &str) -> Result<&str, String> {
    if !path.starts_with('/') || path.contains("..") || path.contains("//") {
        return Err("invalid authenticated request path".to_string());
    }
    Ok(path)
}

pub async fn managed_usage(
    client: &reqwest::Client,
    env: &KimiAuthEnv,
    access_token: &str,
) -> Result<ManagedUsageReport, String> {
    let resp = authenticated_request(client, env, "GET", "/usages", None, access_token).await?;
    if resp.status != 200 {
        let message = api_error_message(&resp.body).unwrap_or_else(|| match resp.status {
            401 => "Authorization failed. Please run `smelt auth`.".to_string(),
            403 => "Usage unavailable for this account.".to_string(),
            404 => "Usage endpoint not available. Try Kimi For Coding.".to_string(),
            _ => "Usage unavailable right now. Try again later.".to_string(),
        });
        return Err(message);
    }
    let payload: Value = serde_json::from_str(&resp.body)
        .map_err(|_| "Usage response was invalid. Try again later.".to_string())?;
    Ok(parse_managed_usage_payload(&payload, (env.now)()))
}

pub fn parse_models_response(json: &serde_json::Value) -> Vec<KimiCodeModelInfo> {
    json["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(KimiCodeModelInfo::from_json)
        .collect()
}

pub async fn fetch_models_with_token(
    client: &reqwest::Client,
    env: &KimiAuthEnv,
    token: &str,
) -> Result<Vec<KimiCodeModelInfo>, String> {
    let resp = apply_default_headers(client.get(format!("{}/models", env.api_base)), &env.headers)
        .bearer_auth(token)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("Kimi Code models request failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "Kimi Code models endpoint rejected credentials (HTTP {status}): {body}"
        ));
    }
    let json = resp
        .json::<serde_json::Value>()
        .await
        .map_err(|e| format!("Kimi Code models response was invalid JSON: {e}"))?;
    Ok(parse_models_response(&json))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_models_response_keeps_kimi_metadata() {
        let json = serde_json::json!({"data": [{
            "id": "moonshot-v1",
            "context_length": 131072,
            "supports_reasoning": false,
            "supports_image_in": true,
            "supports_video_in": false,
            "supports_tool_use": true,
            "display_name": "Moonshot V1"
        }]});

        let models = parse_models_response(&json);

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "moonshot-v1");
        assert_eq!(models[0].context_length, Some(131_072));
        assert_eq!(models[0].supports_reasoning, Some(false));
        assert_eq!(models[0].supports_image_in, Some(true));
        assert_eq!(models[0].supports_video_in, Some(false));
        assert_eq!(models[0].supports_tool_use, Some(true));
        assert!(models[0].matches_name("Moonshot V1"));
    }

    #[test]
    fn parse_managed_usage_payload_matches_kimi_usage_shape() {
        let parsed = parse_managed_usage_payload(
            &serde_json::json!({
                "usage": { "used": 40.9, "limit": "1000", "name": "Weekly limit" },
                "limits": [
                    { "detail": { "used": 1.9, "limit": 100 }, "window": { "duration": 300.0, "timeUnit": "MINUTE" } },
                    { "detail": { "remaining": 10, "limit": 50 }, "window": { "duration": 24, "timeUnit": "HOUR" } },
                    { "name": "Daily cap", "detail": { "used": 5, "limit": 100 }, "window": { "duration": 1440, "timeUnit": "MINUTE" } }
                ]
            }),
            1_000,
        );

        assert_eq!(
            parsed.summary,
            Some(ManagedUsageRow {
                label: "Weekly limit".into(),
                used: 40,
                limit: 1000,
                reset_hint: None,
            })
        );
        assert_eq!(parsed.limits[0].label, "5h limit");
        assert_eq!(parsed.limits[0].used, 1);
        assert_eq!(parsed.limits[1].label, "24h limit");
        assert_eq!(parsed.limits[1].used, 40);
        assert_eq!(parsed.limits[2].label, "Daily cap");
    }

    #[test]
    fn parse_iso_time_honors_timezone_offsets_and_fractional_seconds() {
        assert_eq!(parse_iso_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso_time("1970-01-01T02:00:00.123+02:00"), Some(0));
        assert_eq!(parse_iso_time("1969-12-31T19:00:00-0500"), Some(0));
    }

    #[test]
    fn format_duration_matches_kimi_style() {
        assert_eq!(format_duration(45), "45s");
        assert_eq!(format_duration(90), "1m");
        assert_eq!(format_duration(3661), "1h 1m");
        assert_eq!(format_duration(86_400 + 7200 + 600), "1d 2h 10m");
    }

    #[test]
    fn json_u32_accepts_positive_numbers_and_numeric_strings_only() {
        assert_eq!(json_u32(&serde_json::json!(42)), Some(42));
        assert_eq!(json_u32(&serde_json::json!("42")), Some(42));
        assert_eq!(json_u32(&serde_json::json!(0)), None);
        assert_eq!(json_u32(&serde_json::json!("0")), None);
        assert_eq!(json_u32(&serde_json::json!("nope")), None);
        assert_eq!(json_u32(&serde_json::json!(u64::MAX)), None);
    }

    #[test]
    fn token_from_response_requires_core_fields() {
        let missing_access = token_from_response_at(
            &serde_json::json!({
                "refresh_token": "refresh",
                "expires_in": 60
            }),
            100,
        )
        .unwrap_err();
        assert!(missing_access.contains("access_token"));

        let missing_refresh = token_from_response_at(
            &serde_json::json!({
                "access_token": "access",
                "expires_in": 60
            }),
            100,
        )
        .unwrap_err();
        assert!(missing_refresh.contains("refresh_token"));

        let missing_expiry = token_from_response_at(
            &serde_json::json!({
                "access_token": "access",
                "refresh_token": "refresh"
            }),
            100,
        )
        .unwrap_err();
        assert!(missing_expiry.contains("expires_in"));
    }

    #[test]
    fn token_from_response_uses_supplied_now_and_defaults_optional_fields() {
        let tokens = token_from_response_at(
            &serde_json::json!({
                "access_token": "access",
                "refresh_token": "refresh",
                "expires_in": 60
            }),
            1_000,
        )
        .unwrap();

        assert_eq!(tokens.access_token, "access");
        assert_eq!(tokens.refresh_token, "refresh");
        assert_eq!(tokens.scope, "");
        assert_eq!(tokens.token_type, "Bearer");
        assert_eq!(tokens.expires_at, 1_060);
    }

    #[test]
    fn device_poll_decision_classifies_pending_slowdown_and_terminal_errors() {
        let pending = OAuthResponse {
            status: 400,
            body: String::new(),
            json: serde_json::json!({"error": "authorization_pending"}),
        };
        assert_eq!(
            device_poll_decision(&pending, 5).unwrap(),
            DevicePollDecision::Continue { interval: 5 }
        );

        let slow_down = OAuthResponse {
            status: 400,
            body: String::new(),
            json: serde_json::json!({"error": "slow_down"}),
        };
        assert_eq!(
            device_poll_decision(&slow_down, 5).unwrap(),
            DevicePollDecision::Continue { interval: 10 }
        );

        let expired = OAuthResponse {
            status: 400,
            body: String::new(),
            json: serde_json::json!({"error": "expired_token"}),
        };
        assert_eq!(
            device_poll_decision(&expired, 5).unwrap(),
            DevicePollDecision::Expired
        );
    }

    #[test]
    fn oauth_error_prefers_structured_fields_before_raw_body() {
        let resp = OAuthResponse {
            status: 400,
            body: "raw body".into(),
            json: serde_json::json!({"message": "structured message"}),
        };
        assert_eq!(
            oauth_error(&resp, "Refresh failed"),
            "Refresh failed (HTTP 400): structured message"
        );

        let raw = OAuthResponse {
            status: 500,
            body: "server exploded".into(),
            json: serde_json::Value::Null,
        };
        assert_eq!(
            oauth_error(&raw, "Refresh failed"),
            "Refresh failed (HTTP 500): server exploded"
        );
    }

    fn fixed_now() -> u64 {
        1_000
    }

    fn test_env(oauth_host: String) -> KimiAuthEnv {
        KimiAuthEnv {
            oauth_host,
            api_base: "http://127.0.0.1:9".to_string(),
            headers: KimiHeaders {
                user_agent: "smelt/test".to_string(),
                version: "test".to_string(),
                device_name: "device".to_string(),
                device_model: "model".to_string(),
                os_version: "os".to_string(),
                device_id: "id".to_string(),
            },
            now: fixed_now,
        }
    }

    #[test]
    fn build_oauth_form_request_uses_env_host_and_urlencoded_body() {
        let env = test_env("https://auth.example".to_string());

        let req = build_oauth_form_request(
            &env,
            "/api/oauth/token",
            &[
                ("client_id", "client-1"),
                ("scope", "a b"),
                ("redirect", "x/y"),
            ],
        );

        assert_eq!(req.url, "https://auth.example/api/oauth/token");
        assert_eq!(req.body, "client_id=client-1&scope=a+b&redirect=x%2Fy");
    }
}
