#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiBaseNormalizationHint {
    pub original: String,
    pub normalized: String,
    pub endpoint: &'static str,
}

fn strip_known_endpoint(base: &str) -> Option<(&str, &'static str)> {
    for (suffix, endpoint) in [
        ("/chat/completions", "chat/completions"),
        ("/responses", "responses"),
        ("/messages", "messages"),
    ] {
        if let Some(stripped) = base.strip_suffix(suffix) {
            return Some((stripped.trim_end_matches('/'), endpoint));
        }
    }
    None
}

pub fn api_base_normalization_hint(api_base: &str) -> Option<ApiBaseNormalizationHint> {
    let original = api_base.trim().trim_end_matches('/');
    let (normalized, endpoint) = strip_known_endpoint(original)?;
    Some(ApiBaseNormalizationHint {
        original: original.to_string(),
        normalized: normalized.to_string(),
        endpoint,
    })
}

pub fn normalize_api_base(api_base: &str) -> String {
    api_base_normalization_hint(api_base)
        .map(|hint| hint.normalized)
        .unwrap_or_else(|| api_base.trim().trim_end_matches('/').to_string())
}

pub fn endpoint_url(api_base: &str, endpoint: &str) -> String {
    let base = normalize_api_base(api_base);
    let endpoint = endpoint.trim_start_matches('/');
    format!("{base}/{endpoint}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_url_accepts_base_or_full_endpoint() {
        assert_eq!(
            endpoint_url("https://api.cerebras.ai/v1", "chat/completions"),
            "https://api.cerebras.ai/v1/chat/completions"
        );
        assert_eq!(
            endpoint_url(
                "https://api.cerebras.ai/v1/chat/completions",
                "chat/completions"
            ),
            "https://api.cerebras.ai/v1/chat/completions"
        );
        assert_eq!(
            endpoint_url("https://api.openai.com/v1/responses", "responses"),
            "https://api.openai.com/v1/responses"
        );
        assert_eq!(
            endpoint_url("https://api.anthropic.com/v1/messages/", "messages"),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn api_base_normalization_hint_reports_endpoint_suffix() {
        assert_eq!(
            api_base_normalization_hint(" https://api.cerebras.ai/v1/chat/completions/ "),
            Some(ApiBaseNormalizationHint {
                original: "https://api.cerebras.ai/v1/chat/completions".into(),
                normalized: "https://api.cerebras.ai/v1".into(),
                endpoint: "chat/completions",
            })
        );
        assert_eq!(
            api_base_normalization_hint("https://api.cerebras.ai/v1"),
            None
        );
        assert_eq!(
            api_base_normalization_hint("https://api.cerebras.ai/v1/chat/completions?x=1"),
            None
        );
    }
}
