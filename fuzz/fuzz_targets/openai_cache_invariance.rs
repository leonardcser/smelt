#![no_main]

//! OpenAI / aux-model prompt-cache invariance fuzz. The Anthropic target
//! covers `cache_control`-marker stability; this target covers the
//! OpenAI Responses shape used by aux models (title generation, prompt
//! prediction) — `instructions` (joined system prompts), `input` array,
//! `tools`, and the session-scoped `prompt_cache_key` routing hint.
//!
//! Cache-invariance properties under OpenAI semantics:
//!  - `instructions` bytes must not drift across stable actions.
//!  - `input` array prefix must stay byte-identical (the cached prefix
//!    is implicit on OpenAI; the routing hint just lands the request on
//!    the shard that already saw it).
//!  - `prompt_cache_key` must stay byte-identical across stable actions
//!    within a session — a drift here invalidates the routing hint and
//!    burns the aux cache.
//!  - `tools` must be sorted by `sort_tools_for_cache_stability` so
//!    registration-order drift can't bust the prefix.
//!
//! Each iteration: build a baseline OpenAI body with a fixed
//! `prompt_cache_key`, apply one stable action, build the post-action
//! body, assert the cache-relevant fields are byte-identical.

use arbitrary::Arbitrary;
use engine::provider::{
    fuzz_build_openai_body, sort_tools_for_cache_stability, CacheConfig, FunctionSchema,
    ToolDefinition,
};
use engine::ModelConfig;
use libfuzzer_sys::fuzz_target;
use protocol::{mode_change_note, AgentMode, Content, Message, ReasoningEffort};
use serde_json::Value;

#[derive(Debug, Clone, Arbitrary)]
struct ArbTool {
    name_idx: u8,
    description: String,
}

const TOOL_NAMES: &[&str] = &[
    "read", "write", "edit", "grep", "glob", "ls", "bash", "fetch", "spawn",
];

impl ArbTool {
    fn build(&self) -> ToolDefinition {
        let name = TOOL_NAMES[(self.name_idx as usize) % TOOL_NAMES.len()].to_string();
        ToolDefinition::new(FunctionSchema {
            name,
            description: self.description.clone(),
            parameters: serde_json::json!({"type": "object"}),
        })
    }
}

#[derive(Debug, Clone, Arbitrary)]
enum StableAction {
    AppendTurn {
        assistant_text: String,
        user_text: String,
    },
    AppendModeNote {
        mode: u8,
        user_text: String,
    },
    ReorderTools,
    NudgeReasoningEffort,
}

#[derive(Debug, Arbitrary)]
struct Input {
    /// First system message — `instructions` concatenates all `Role::System`
    /// contents with `\n`, so we also seed a few extras.
    initial_system: String,
    extra_system: Vec<String>,
    initial_user_texts: Vec<String>,
    tools: Vec<ArbTool>,
    action: StableAction,
    /// Stable session id seed; clamped to a non-empty 64-char-max key
    /// inside the body builder.
    session_key: String,
}

const MODES: &[AgentMode] = &[
    AgentMode::Normal,
    AgentMode::Plan,
    AgentMode::Apply,
    AgentMode::Yolo,
];

fn build_history(input: &Input) -> Vec<Message> {
    let mut msgs = Vec::with_capacity(input.initial_user_texts.len() * 2 + 4);
    msgs.push(Message::system(input.initial_system.clone()));
    for sys in input.extra_system.iter().take(2) {
        msgs.push(Message::system(sys.clone()));
    }
    for (i, text) in input.initial_user_texts.iter().enumerate().take(8) {
        msgs.push(Message::user(Content::text(text.clone())));
        if i + 1 < input.initial_user_texts.len().min(8) {
            msgs.push(Message::assistant(
                Some(Content::text(format!("ack {i}"))),
                None,
                None,
            ));
        }
    }
    msgs
}

fn cache_with_key(key: &str) -> CacheConfig {
    CacheConfig {
        anthropic_markers: false,
        ttl_long: false,
        prompt_cache_key: Some(key.to_string()),
    }
}

fn dedup_arb_tools_by_name(arb: &[ArbTool]) -> Vec<ArbTool> {
    let mut seen = std::collections::HashSet::new();
    arb.iter()
        .take(6)
        .filter(|t| seen.insert(t.name_idx as usize % TOOL_NAMES.len()))
        .cloned()
        .collect()
}

fn build_tools(arb: &[ArbTool]) -> Vec<ToolDefinition> {
    let mut tools: Vec<ToolDefinition> = dedup_arb_tools_by_name(arb)
        .iter()
        .map(|t| t.build())
        .collect();
    sort_tools_for_cache_stability(&mut tools);
    tools
}

fn body(
    messages: &[Message],
    tools: &[ToolDefinition],
    cfg: &ModelConfig,
    cache: &CacheConfig,
    effort: ReasoningEffort,
) -> Value {
    fuzz_build_openai_body(messages, tools, "m", effort, cfg, cache)
}

fn assert_field_eq(before: &Value, after: &Value, field: &str, label: &str) {
    if before.get(field) != after.get(field) {
        panic!(
            "OPENAI CACHE: `{field}` drifted ({label})\n  before: {}\n   after: {}",
            serde_json::to_string(before.get(field).unwrap_or(&Value::Null)).unwrap_or_default(),
            serde_json::to_string(after.get(field).unwrap_or(&Value::Null)).unwrap_or_default()
        );
    }
}

fn assert_prefix_stable(
    before: &Value,
    after: &Value,
    prefix_input_count: usize,
    label: &str,
    check_full_input: bool,
) {
    assert_field_eq(before, after, "instructions", label);
    assert_field_eq(before, after, "tools", label);
    assert_field_eq(before, after, "prompt_cache_key", label);

    let empty = Vec::new();
    let bi = before
        .get("input")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);
    let ai = after
        .get("input")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);

    if check_full_input && bi.len() != ai.len() {
        panic!(
            "OPENAI CACHE: input length drifted ({label}): before={} after={}",
            bi.len(),
            ai.len()
        );
    }
    let n = prefix_input_count.min(bi.len()).min(ai.len());
    for i in 0..n {
        if bi[i] != ai[i] {
            panic!(
                "OPENAI CACHE: input[{i}] drifted ({label})\n  before: {}\n   after: {}",
                serde_json::to_string(&bi[i]).unwrap_or_default(),
                serde_json::to_string(&ai[i]).unwrap_or_default()
            );
        }
    }
}

fn run(input: Input) {
    let messages = build_history(&input);
    if messages.len() < 2 {
        return;
    }
    let tools = build_tools(&input.tools);
    let cfg = ModelConfig::default();

    // Session key is sticky across the whole session — fix it once and
    // expect every subsequent body to carry the same clamped value.
    let key = if input.session_key.is_empty() {
        "session".to_string()
    } else {
        input.session_key.clone()
    };
    let cache = cache_with_key(&key);

    let before = body(&messages, &tools, &cfg, &cache, ReasoningEffort::Off);

    // `input` items: system messages collapse into `instructions`, so
    // the array's first index is the first non-system message.
    let nonsystem_prefix = messages
        .iter()
        .filter(|m| m.role != protocol::Role::System)
        .count();

    let mut after_messages = messages.clone();
    match &input.action {
        StableAction::AppendTurn {
            assistant_text,
            user_text,
        } => {
            after_messages.push(Message::assistant(
                Some(Content::text(assistant_text.clone())),
                None,
                None,
            ));
            after_messages.push(Message::user(Content::text(user_text.clone())));
            let after = body(&after_messages, &tools, &cfg, &cache, ReasoningEffort::Off);
            assert_prefix_stable(&before, &after, nonsystem_prefix, "AppendTurn", false);
        }
        StableAction::AppendModeNote { mode, user_text } => {
            let m = MODES[(*mode as usize) % MODES.len()];
            after_messages.push(Message::assistant(
                Some(Content::text(String::from("ok"))),
                None,
                None,
            ));
            after_messages.push(Message::user(Content::text(mode_change_note(m))));
            after_messages.push(Message::user(Content::text(user_text.clone())));
            let after = body(&after_messages, &tools, &cfg, &cache, ReasoningEffort::Off);
            assert_prefix_stable(&before, &after, nonsystem_prefix, "AppendModeNote", false);
        }
        StableAction::ReorderTools => {
            let mut reordered_arb: Vec<ArbTool> = dedup_arb_tools_by_name(&input.tools)
                .into_iter()
                .rev()
                .collect();
            reordered_arb = dedup_arb_tools_by_name(&reordered_arb);
            let mut reordered: Vec<ToolDefinition> =
                reordered_arb.iter().map(|t| t.build()).collect();
            sort_tools_for_cache_stability(&mut reordered);
            let after = body(&messages, &reordered, &cfg, &cache, ReasoningEffort::Off);
            assert_prefix_stable(&before, &after, nonsystem_prefix, "ReorderTools", true);
        }
        StableAction::NudgeReasoningEffort => {
            // Reasoning effort sits in `reasoning`/`include`, not in the
            // cached prefix. `instructions` / `input` / `tools` /
            // `prompt_cache_key` must stay byte-identical.
            let after = body(&messages, &tools, &cfg, &cache, ReasoningEffort::Low);
            assert_prefix_stable(
                &before,
                &after,
                nonsystem_prefix,
                "NudgeReasoningEffort",
                true,
            );
        }
    }
}

fuzz_target!(|input: Input| {
    run(input);
});
