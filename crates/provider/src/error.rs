use std::time::Duration;

#[derive(Debug, Clone, thiserror::Error)]
pub enum ProviderError {
    #[error("cancelled")]
    Cancelled,
    #[error("{}", format_rate_limit(resets_at))]
    RateLimited { resets_at: Option<u64> },
    #[error("{}", quota_exceeded_message())]
    QuotaExceeded {
        body: String,
        resets_at: Option<u64>,
    },
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("server error {status}: {body}")]
    Server { status: u16, body: String },
    #[error("network error: {0}")]
    Network(String),
    #[error("stream error: {0}")]
    Stream(String),
    #[error("invalid response: {0}")]
    InvalidResponse(String),
    #[error("max retries exceeded")]
    MaxRetries,
}

pub fn quota_exceeded_message() -> &'static str {
    "API quota exceeded; check your plan and billing details"
}

fn is_quota_error_body(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("insufficient_quota")
        || lower.contains("billing_not_active")
        || lower.contains("credit balance is too low")
        || lower.contains("usage_limit_reached")
        || lower.contains("usage limit")
        || lower.contains("quota exceeded")
        || lower.contains("quota exhausted")
}

const MAX_AUTO_RATE_LIMIT_DELAY: Duration = Duration::from_secs(10 * 60);

pub fn rate_limit_error(
    resets_at: Option<u64>,
    retry_after: Option<Duration>,
    now_secs: u64,
) -> ProviderError {
    ProviderError::RateLimited {
        resets_at: resets_at.or_else(|| retry_after.map(|d| now_secs + d.as_secs())),
    }
}

pub fn retry_delay_for(
    err: &ProviderError,
    attempt: usize,
    retry_after: Option<Duration>,
    now_secs: u64,
) -> Option<Duration> {
    let backoff = backoff_delay(attempt);
    match err {
        ProviderError::Network(_) | ProviderError::Server { .. } | ProviderError::Stream(_) => {
            Some(retry_after.map_or(backoff, |delay| delay.max(backoff)))
        }
        ProviderError::RateLimited {
            resets_at: Some(epoch),
        } => {
            let delay = Duration::from_secs(epoch.saturating_sub(now_secs));
            (delay <= MAX_AUTO_RATE_LIMIT_DELAY).then_some(delay.max(backoff))
        }
        _ => None,
    }
}

pub fn format_rate_limit(resets_at: &Option<u64>) -> String {
    let Some(epoch) = resets_at else {
        return "rate limited".to_string();
    };
    let time_str = format_epoch_local(*epoch);
    format!("rate limited; try again at {time_str}")
}

pub fn format_epoch_local(epoch_secs: u64) -> String {
    #[cfg(unix)]
    {
        let t = epoch_secs as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe { libc::localtime_r(&t, &mut tm) };

        const MONTHS: [&str; 12] = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        let month = MONTHS[tm.tm_mon as usize % 12];
        let day = tm.tm_mday;
        let year = tm.tm_year + 1900;
        let suffix = match day % 10 {
            1 if day != 11 => "st",
            2 if day != 12 => "nd",
            3 if day != 13 => "rd",
            _ => "th",
        };
        let (hour12, ampm) = match tm.tm_hour {
            0 => (12, "AM"),
            1..=11 => (tm.tm_hour, "AM"),
            12 => (12, "PM"),
            _ => (tm.tm_hour - 12, "PM"),
        };
        format!(
            "{month} {day}{suffix}, {year} {hour12}:{:02} {ampm}",
            tm.tm_min
        )
    }
    #[cfg(not(unix))]
    {
        let _ = epoch_secs;
        "later".to_string()
    }
}

pub fn unix_now() -> u64 {
    unix_secs(std::time::SystemTime::now())
}

pub fn unix_secs(time: std::time::SystemTime) -> u64 {
    time.duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl ProviderError {
    pub fn from_http_at(
        code: u16,
        body: String,
        retry_after: Option<Duration>,
        now_secs: u64,
    ) -> Self {
        let is_quota = is_quota_error_body(&body);

        match code {
            _ if is_quota => ProviderError::QuotaExceeded {
                resets_at: parse_resets_at(&body)
                    .or_else(|| retry_after.map(|d| now_secs + d.as_secs())),
                body,
            },
            400 => ProviderError::InvalidResponse(body),
            401 | 403 => ProviderError::Auth(body),
            404 => ProviderError::NotFound(body),
            429 => rate_limit_error(parse_resets_at(&body), retry_after, now_secs),
            _ => ProviderError::Server { status: code, body },
        }
    }

    pub fn from_http(code: u16, body: String, retry_after: Option<Duration>) -> Self {
        Self::from_http_at(code, body, retry_after, unix_now())
    }
}

pub fn parse_resets_at(body: &str) -> Option<u64> {
    let json: serde_json::Value = serde_json::from_str(body).ok()?;
    json.get("error")
        .and_then(|e| e.get("resets_at"))
        .and_then(json_as_u64)
}

pub fn json_as_u64(v: &serde_json::Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_i64().and_then(|i| u64::try_from(i).ok()))
}

pub fn parse_retry_from_body(body: &str) -> Option<Duration> {
    let lower = body.to_ascii_lowercase();
    let idx = lower.find("try again in")?;
    let after = &lower[idx + "try again in".len()..];
    let trimmed = after.trim_start();

    let end = trimmed
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(trimmed.len());
    let value: f64 = trimmed[..end].parse().ok()?;

    let unit = trimmed[end..].trim_start();
    if unit.starts_with("ms") {
        Some(Duration::from_millis(value as u64))
    } else {
        Some(Duration::from_secs_f64(value))
    }
}

pub fn backoff_delay(attempt: usize) -> Duration {
    Duration::from_millis(500 * 2u64.pow(attempt as u32))
}

pub fn parse_retry_after(resp: &reqwest::Response) -> Option<Duration> {
    let val = resp.headers().get("retry-after")?.to_str().ok()?;
    val.parse::<f64>()
        .ok()
        .filter(|&s| s > 0.0)
        .map(Duration::from_secs_f64)
}
