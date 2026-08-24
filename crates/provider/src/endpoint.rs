#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiBaseNormalizationHint {
    pub original: String,
    pub normalized: String,
    pub endpoint: &'static str,
}

pub(crate) fn trim_api_base(api_base: &str) -> &str {
    let base = smelt_buffer::text::trim_start_whitespace(api_base);
    let end = smelt_buffer::cell_width::grapheme_indices(base)
        .rev()
        .take_while(|(_, grapheme)| *grapheme == "/" || grapheme.chars().all(char::is_whitespace))
        .map(|(start, _)| start)
        .last()
        .unwrap_or(base.len());
    &base[..end]
}

fn strip_known_endpoint(base: &str) -> Option<(&str, &'static str)> {
    for (suffix, endpoint) in [
        ("/chat/completions", "chat/completions"),
        ("/responses", "responses"),
        ("/messages", "messages"),
    ] {
        if let Some(stripped) = base.strip_suffix(suffix) {
            return Some((trim_api_base(stripped), endpoint));
        }
    }
    None
}

fn strip_known_endpoints(base: &str) -> Option<(&str, &'static str)> {
    let (mut normalized, endpoint) = strip_known_endpoint(base)?;
    while let Some((next, _)) = strip_known_endpoint(normalized) {
        normalized = next;
    }
    Some((normalized, endpoint))
}

pub fn api_base_normalization_hint(api_base: &str) -> Option<ApiBaseNormalizationHint> {
    let original = trim_api_base(api_base);
    let (normalized, endpoint) = strip_known_endpoints(original)?;
    Some(ApiBaseNormalizationHint {
        original: original.to_string(),
        normalized: normalized.to_string(),
        endpoint,
    })
}

pub fn normalize_api_base(api_base: &str) -> String {
    api_base_normalization_hint(api_base)
        .map(|hint| hint.normalized)
        .unwrap_or_else(|| trim_api_base(api_base).to_string())
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
    fn api_base_trimming_keeps_graphemes_atomic() {
        assert_eq!(
            normalize_api_base(" \u{301}host\u{600}/"),
            " \u{301}host\u{600}/"
        );
    }

    #[test]
    fn api_base_normalization_is_idempotent() {
        for (input, expected) in [
            (" /go / ", "/go"),
            (
                "https://example.test/v1/responses/messages",
                "https://example.test/v1",
            ),
        ] {
            let normalized = normalize_api_base(input);
            assert_eq!(normalized, expected);
            assert_eq!(normalize_api_base(&normalized), expected);
        }
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
