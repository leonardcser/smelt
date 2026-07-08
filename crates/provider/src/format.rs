use crate::{parse_claude_model_version, WireApi};

/// Structured JSON output schema. Each provider adapter maps this to its native field.
#[derive(Clone)]
pub struct ResponseFormat {
    pub name: String,
    pub schema: serde_json::Value,
}

pub fn apply_response_format(body: &mut serde_json::Value, wire: WireApi, fmt: &ResponseFormat) {
    match wire {
        WireApi::ChatCompletions => {
            body["response_format"] = serde_json::json!({
                "type": "json_schema",
                "json_schema": {
                    "name": fmt.name,
                    "schema": fmt.schema,
                    "strict": true,
                }
            });
        }
        WireApi::OpenAiResponses => {
            body["text"] = serde_json::json!({
                "format": {
                    "type": "json_schema",
                    "name": fmt.name,
                    "schema": fmt.schema,
                    "strict": true,
                }
            });
        }
        WireApi::AnthropicMessages => {
            // Older models (Haiku 3.5, Sonnet 3.7, etc.) 400 if this field is sent.
            let model = body["model"].as_str().unwrap_or("");
            if !anthropic_supports_structured_output(model) {
                return;
            }
            let format_val = serde_json::json!({
                "type": "json_schema",
                "schema": fmt.schema,
            });
            match body.get_mut("output_config") {
                Some(v) if v.is_object() => {
                    v["format"] = format_val;
                }
                _ => {
                    body["output_config"] = serde_json::json!({ "format": format_val });
                }
            }
        }
    }
}

pub fn anthropic_supports_structured_output(model: &str) -> bool {
    model.contains("mythos")
        || parse_claude_model_version(model).is_some_and(|version| version.at_least(4, 5))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fmt() -> ResponseFormat {
        ResponseFormat {
            name: "out".into(),
            schema: json!({"type":"object"}),
        }
    }

    #[test]
    fn apply_response_format_openai_compatible_writes_response_format_json_schema() {
        let mut body = json!({});
        apply_response_format(&mut body, WireApi::ChatCompletions, &fmt());
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["name"], "out");
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
    }

    #[test]
    fn apply_response_format_openai_writes_text_format_block() {
        let mut body = json!({});
        apply_response_format(&mut body, WireApi::OpenAiResponses, &fmt());
        assert_eq!(body["text"]["format"]["type"], "json_schema");
        assert_eq!(body["text"]["format"]["name"], "out");
    }

    #[test]
    fn apply_response_format_anthropic_modern_model_creates_output_config_format() {
        let mut body = json!({"model": "claude-sonnet-4-6"});
        apply_response_format(&mut body, WireApi::AnthropicMessages, &fmt());
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
    }

    #[test]
    fn apply_response_format_anthropic_merges_into_existing_output_config_object() {
        let mut body = json!({"model": "claude-opus-4-6", "output_config": {"effort": "high"}});
        apply_response_format(&mut body, WireApi::AnthropicMessages, &fmt());
        assert_eq!(body["output_config"]["effort"], "high");
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
    }

    #[test]
    fn apply_response_format_anthropic_legacy_model_does_not_write_field() {
        let mut body = json!({"model": "claude-3-5-sonnet"});
        apply_response_format(&mut body, WireApi::AnthropicMessages, &fmt());
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn anthropic_supports_structured_output_recognizes_4_5_4_6_and_mythos() {
        for model in [
            "claude-sonnet-4-5-20250929",
            "claude-opus-4-6",
            "claude-mythos-20250414",
        ] {
            assert!(anthropic_supports_structured_output(model), "{model}");
        }
    }

    #[test]
    fn anthropic_supports_structured_output_rejects_older_models() {
        for model in ["claude-3-5-sonnet", "claude-sonnet-4-4", "gpt-5"] {
            assert!(!anthropic_supports_structured_output(model), "{model}");
        }
    }
}
