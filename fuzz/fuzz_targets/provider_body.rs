#![no_main]

//! Provider body construction fuzzing. This keeps provider wire-format/cache
//! decisions in a low-dependency target instead of relying on the TUI shell to
//! accidentally reach them.

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use protocol::{Content, Message, ModelConfig, ReasoningEffort};
use smelt_fuzz::cache_common::{build_tools, ArbTool};
use smelt_provider::{
    fuzz_api_key_auth, fuzz_build_anthropic_body, fuzz_build_chat_completions_body,
    fuzz_build_openai_body, fuzz_extract_tool_calls, fuzz_parse_catalog, normalize_api_base,
    CacheConfig, ProviderKind,
};

#[derive(Arbitrary, Debug)]
struct Input {
    system: String,
    texts: Vec<String>,
    tools: Vec<ArbTool>,
    model: String,
    effort: u8,
    cache_key: Option<String>,
    ttl_long: bool,
    provider_type: String,
    api_base: String,
    api_key: String,
    catalog_json: String,
    extract_text: String,
}

fn run(input: Input) {
    assert_provider_helpers(&input);
    let expected_message_count = input.texts.len().min(12);
    let expected_cache_key_chars = input.cache_key.as_ref().map(|s| s.chars().take(64).count());
    let mut messages = Vec::new();
    messages.push(Message::system(input.system));
    for (i, text) in input.texts.into_iter().take(12).enumerate() {
        if i % 3 == 2 {
            messages.push(Message::assistant(Some(Content::text(text)), None, None));
        } else {
            messages.push(Message::user(Content::text(text)));
        }
    }
    let tools = build_tools(&input.tools);
    let cfg = ModelConfig::default();
    let effort = match input.effort % 4 {
        0 => ReasoningEffort::Off,
        1 => ReasoningEffort::Low,
        2 => ReasoningEffort::Medium,
        _ => ReasoningEffort::High,
    };
    let cache = CacheConfig {
        anthropic_markers: true,
        ttl_long: input.ttl_long,
        prompt_cache_key: input.cache_key,
    };
    let model = if input.model.is_empty() {
        "fuzz"
    } else {
        &input.model
    };

    let anthropic = fuzz_build_anthropic_body(&messages, &tools, model, effort, &cfg, &cache);
    let chat = fuzz_build_chat_completions_body(&messages, &tools, model, effort, &cfg);
    let openai = fuzz_build_openai_body(&messages, &tools, model, effort, &cfg, &cache);

    assert_provider_body_schema(
        &anthropic,
        &chat,
        &openai,
        model,
        expected_message_count,
        tools.len(),
        effort,
        expected_cache_key_chars,
    );
}

fn assert_provider_body_schema(
    anthropic: &serde_json::Value,
    chat: &serde_json::Value,
    openai: &serde_json::Value,
    model: &str,
    expected_message_count: usize,
    expected_tool_count: usize,
    effort: ReasoningEffort,
    expected_cache_key_chars: Option<usize>,
) {
    assert_eq!(anthropic["model"], model, "anthropic body model changed");
    assert_eq!(chat["model"], model, "chat completions body model changed");
    assert_eq!(openai["model"], model, "openai body model changed");

    let anthropic_messages = anthropic["messages"]
        .as_array()
        .expect("anthropic messages is an array");
    assert_eq!(
        anthropic_messages.len(),
        expected_message_count,
        "anthropic message count changed"
    );
    for message in anthropic_messages {
        assert!(
            matches!(message["role"].as_str(), Some("user" | "assistant")),
            "anthropic message has invalid role: {message:?}"
        );
        assert!(
            message["content"].is_array(),
            "anthropic message content is not an array"
        );
    }

    let chat_messages = chat["messages"]
        .as_array()
        .expect("chat messages is an array");
    assert_eq!(
        chat_messages.len(),
        expected_message_count + 1,
        "chat completions message count changed"
    );
    for message in chat_messages {
        assert!(
            matches!(
                message["role"].as_str(),
                Some("system" | "user" | "assistant" | "tool")
            ),
            "chat completions message has invalid role: {message:?}"
        );
        let allowed = match message["role"].as_str() {
            Some("assistant") => &["role", "content", "tool_calls"][..],
            Some("tool") => &["role", "content", "tool_call_id"][..],
            _ => &["role", "content"][..],
        };
        assert!(
            message
                .as_object()
                .is_some_and(|object| object.keys().all(|key| allowed.contains(&key.as_str()))),
            "chat completions message leaked internal fields: {message:?}"
        );
    }

    let openai_input = openai["input"]
        .as_array()
        .expect("openai input is an array");
    assert_eq!(
        openai_input.len(),
        expected_message_count,
        "openai input count changed"
    );
    for item in openai_input {
        let item_type = item["type"].as_str();
        let role = item["role"].as_str();
        assert!(
            role.is_some()
                || matches!(
                    item_type,
                    Some("function_call" | "function_call_output" | "reasoning")
                ),
            "openai input item lost role/type identity: {item:?}"
        );
    }

    assert_tool_array(anthropic.get("tools"), expected_tool_count, "anthropic");
    assert_chat_tool_array(chat.get("tools"), expected_tool_count);
    assert_tool_array(openai.get("tools"), expected_tool_count, "openai");

    assert!(
        count_cache_controls(anthropic) <= 4,
        "anthropic body has more than four cache_control markers: {anthropic:?}"
    );
    assert!(
        anthropic.get("prompt_cache_key").is_none(),
        "anthropic body leaked OpenAI prompt_cache_key"
    );
    match expected_cache_key_chars {
        Some(chars) => {
            let key = openai["prompt_cache_key"]
                .as_str()
                .expect("openai body missing prompt_cache_key");
            assert_eq!(
                key.chars().count(),
                chars,
                "openai prompt_cache_key clamp length changed"
            );
        }
        None => assert!(
            openai.get("prompt_cache_key").is_none(),
            "openai body unexpectedly set prompt_cache_key"
        ),
    }

    if effort == ReasoningEffort::Off {
        assert!(
            chat.get("reasoning_effort").is_none(),
            "chat completions body set reasoning while effort is off"
        );
        assert!(
            openai.get("reasoning").is_none(),
            "openai body set reasoning while effort is off"
        );
    } else {
        assert_eq!(
            chat["reasoning_effort"],
            effort.label(),
            "chat completions reasoning effort changed"
        );
        assert!(
            openai.get("reasoning").is_some(),
            "openai body omitted reasoning while effort is enabled"
        );
    }
}

fn assert_tool_array(
    value: Option<&serde_json::Value>,
    expected_tool_count: usize,
    provider: &str,
) {
    if expected_tool_count == 0 {
        assert!(value.is_none(), "{provider} body emitted empty tools array");
        return;
    }
    let tools = value
        .and_then(serde_json::Value::as_array)
        .unwrap_or_else(|| panic!("{provider} tools is not an array"));
    assert_eq!(
        tools.len(),
        expected_tool_count,
        "{provider} tool count changed"
    );
    let mut names = Vec::with_capacity(tools.len());
    for tool in tools {
        let name = tool["name"]
            .as_str()
            .unwrap_or_else(|| panic!("{provider} tool without name: {tool:?}"));
        assert!(!name.is_empty(), "{provider} tool has empty name");
        names.push(name);
    }
    assert!(
        names.windows(2).all(|pair| pair[0] <= pair[1]),
        "{provider} tools are not sorted by name: {names:?}"
    );
}

fn assert_chat_tool_array(value: Option<&serde_json::Value>, expected: usize) {
    if expected == 0 {
        assert!(
            value.is_none(),
            "chat completions body emitted empty tools array"
        );
        return;
    }
    let tools = value
        .and_then(serde_json::Value::as_array)
        .expect("chat completions tools is an array");
    assert_eq!(tools.len(), expected, "chat completions tool count changed");
    for tool in tools {
        assert_eq!(tool["type"], "function");
        assert!(
            tool["function"]["name"]
                .as_str()
                .is_some_and(|name| !name.is_empty()),
            "chat completions tool has no function name: {tool:?}"
        );
    }
}

fn assert_provider_helpers(input: &Input) {
    let normalized = normalize_api_base(&input.api_base);
    assert_eq!(
        normalize_api_base(&normalized),
        normalized,
        "API base normalization is not idempotent"
    );

    let configured = ProviderKind::from_config_and_url(&input.provider_type, &input.api_base);
    let normalized_kind = ProviderKind::from_config_and_url(&input.provider_type, &normalized);
    assert_eq!(
        configured, normalized_kind,
        "API normalization changed provider routing"
    );
    let auth = fuzz_api_key_auth(configured, &input.api_key);
    if input.api_key.is_empty() {
        assert!(auth.is_none(), "empty API key produced auth configuration");
    } else {
        assert_eq!(
            auth,
            configured.descriptor().api_key_auth,
            "API key auth diverged from provider descriptor"
        );
    }

    let _ = fuzz_parse_catalog(&input.catalog_json);
    let (_calls, cleaned) = fuzz_extract_tool_calls(&input.extract_text);
    if let Some(cleaned) = cleaned {
        let (remaining_calls, cleaned_again) = fuzz_extract_tool_calls(&cleaned);
        assert_eq!(
            remaining_calls, 0,
            "cleaned provider text still contains tool calls"
        );
        assert_eq!(cleaned_again.as_deref(), Some(cleaned.as_str()));
    }
}

fn count_cache_controls(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Object(map) => {
            usize::from(map.contains_key("cache_control"))
                + map.values().map(count_cache_controls).sum::<usize>()
        }
        serde_json::Value::Array(values) => values.iter().map(count_cache_controls).sum(),
        _ => 0,
    }
}

fuzz_target!(|input: Input| run(input));
