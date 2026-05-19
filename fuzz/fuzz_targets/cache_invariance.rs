#![no_main]

//! Cache prefix invariance fuzz. Anthropic's prompt cache hits only when
//! the byte prefix of `system` + `tools` + earlier `messages` matches
//! exactly across consecutive requests; a one-byte drift caused by, say,
//! a `/mode` switch that re-rendered the system prompt with a flipped
//! field order silently invalidates the cache and burns tokens.
//!
//! The provider module ships hand-written tests
//! (`crates/engine/src/provider/anthropic.rs::cache_*`) that lock down
//! specific scenarios. This target fuzzes the *property* over random
//! histories: actions that are *supposed* to be cache-stable must leave
//! the prefix bytes byte-identical (ignoring the moving `cache_control`
//! marker, whose position legitimately drifts with the conversation).
//!
//! Stable actions covered here:
//!  - Append-only turn growth (`assistant` + new `user` after the
//!    current last user message).
//!  - Mode-change synthetic user note (`[smelt:mode] ...`) — appended
//!    only, system bytes must stay put.
//!  - Temperature / sampling-param drift in `ModelConfig` — params sit
//!    outside the cached prefix; `system`, `tools`, prior `messages`
//!    must stay byte-identical.
//!  - Tool list reorder — `sort_tools_for_cache_stability` makes the
//!    output position-independent.
//!
//! Each iteration: build a baseline body, apply one stable action,
//! build the post-action body, strip `cache_control` markers from both,
//! assert byte equality of the parts that must stay cached. A
//! divergence means a real bug: tokens are being re-billed.

use arbitrary::Arbitrary;
use engine::provider::{
    fuzz_build_anthropic_body, sort_tools_for_cache_stability, CacheConfig, FunctionSchema,
    ToolDefinition,
};
use engine::ModelConfig;
use libfuzzer_sys::fuzz_target;
use protocol::{Content, Message, ReasoningEffort};
use serde_json::Value;

/// One tool definition, hand-rolled rather than `derive(Arbitrary)` so
/// we can keep the parameters schema valid JSON (random `Value`s would
/// produce mostly garbage that doesn't exercise the cache machinery).
#[derive(Debug, Clone, Arbitrary)]
struct ArbTool {
    /// Index into `TOOL_NAMES` mod its length — bounded so two histories
    /// can collide on the same tool name and the dedup-on-name behavior
    /// is exercised.
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

/// One stable action that must NOT invalidate the cache prefix.
#[derive(Debug, Clone, Arbitrary)]
enum StableAction {
    /// Append `assistant_text("a"), user("u")` — the canonical follow-up
    /// turn. `text` is the new user content.
    AppendTurn { assistant_text: String, user_text: String },
    /// Append a `[smelt:mode]` synthetic note + a regular user turn.
    /// Mirrors `/mode` switching: the synthetic note sits in the message
    /// stream, NOT in the system prompt.
    AppendModeNote { mode: u8, user_text: String },
    /// Reorder tools (shuffle) — the canonical sort must produce the
    /// same output regardless of input order.
    ReorderTools,
    /// Vary sampling params (temperature) — these don't enter the
    /// cached prefix, so `system` / `tools` / `messages` must stay
    /// byte-identical.
    NudgeTemperature,
}

#[derive(Debug, Arbitrary)]
struct Input {
    initial_system: String,
    /// Sequence of (role, text) user/assistant pairs to form the
    /// baseline history.
    initial_user_texts: Vec<String>,
    tools: Vec<ArbTool>,
    action: StableAction,
}

const MODES: &[&str] = &["plan", "yolo", "ask"];

fn strip_markers(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, val) in map {
                if k == "cache_control" {
                    continue;
                }
                out.insert(k.clone(), strip_markers(val));
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(strip_markers).collect()),
        other => other.clone(),
    }
}

fn build_history(input: &Input) -> Vec<Message> {
    let mut msgs = Vec::with_capacity(input.initial_user_texts.len() * 2 + 1);
    msgs.push(Message::system(input.initial_system.clone()));
    for (i, text) in input.initial_user_texts.iter().enumerate().take(8) {
        msgs.push(Message::user(Content::text(text.clone())));
        if i + 1 < input.initial_user_texts.len().min(8) {
            // Interleave assistant responses except after the last user
            // (otherwise the "last user message" position would shift in
            // a way the cache breakpoint logic doesn't model).
            msgs.push(Message::assistant(
                Some(Content::text(format!("ack {i}"))),
                None,
                None,
            ));
        }
    }
    msgs
}

fn cache_on() -> CacheConfig {
    CacheConfig {
        anthropic_markers: true,
        ttl_long: false,
        prompt_cache_key: None,
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

fn body(messages: &[Message], tools: &[ToolDefinition], cfg: &ModelConfig) -> Value {
    fuzz_build_anthropic_body(messages, tools, "m", ReasoningEffort::Off, cfg, &cache_on())
}

fn assert_prefix_stable(before: &Value, after: &Value, prefix_msg_count: usize, label: &str) {
    let b = strip_markers(before);
    let a = strip_markers(after);
    if b.get("system") != a.get("system") {
        panic!(
            "CACHE: system bytes drifted ({label})\n  before: {}\n   after: {}",
            serde_json::to_string(b.get("system").unwrap_or(&Value::Null)).unwrap_or_default(),
            serde_json::to_string(a.get("system").unwrap_or(&Value::Null)).unwrap_or_default()
        );
    }
    if b.get("tools") != a.get("tools") {
        panic!(
            "CACHE: tools bytes drifted ({label})\n  before: {}\n   after: {}",
            serde_json::to_string(b.get("tools").unwrap_or(&Value::Null)).unwrap_or_default(),
            serde_json::to_string(a.get("tools").unwrap_or(&Value::Null)).unwrap_or_default()
        );
    }
    // Check that the first `prefix_msg_count` messages are byte-identical.
    let empty = Vec::new();
    let bm = b.get("messages").and_then(|v| v.as_array()).unwrap_or(&empty);
    let am = a.get("messages").and_then(|v| v.as_array()).unwrap_or(&empty);
    let n = prefix_msg_count.min(bm.len()).min(am.len());
    for i in 0..n {
        if bm[i] != am[i] {
            panic!(
                "CACHE: message[{i}] drifted ({label})\n  before: {}\n   after: {}",
                serde_json::to_string(&bm[i]).unwrap_or_default(),
                serde_json::to_string(&am[i]).unwrap_or_default()
            );
        }
    }
}

fn run(input: Input) {
    let mut messages = build_history(&input);
    if messages.len() < 2 {
        return; // Nothing to cache.
    }
    let tools = build_tools(&input.tools);
    let cfg = ModelConfig::default();

    let before = body(&messages, &tools, &cfg);
    // `prefix_msg_count` = every existing message survives in the prefix.
    // Even when we append, the prior ones must stay byte-identical so the
    // moving breakpoint at the previous-last-user still lands on the
    // same cached prefix.
    let prefix_msg_count = messages.len();

    match &input.action {
        StableAction::AppendTurn {
            assistant_text,
            user_text,
        } => {
            messages.push(Message::assistant(
                Some(Content::text(assistant_text.clone())),
                None,
                None,
            ));
            messages.push(Message::user(Content::text(user_text.clone())));
            let after = body(&messages, &tools, &cfg);
            assert_prefix_stable(&before, &after, prefix_msg_count, "AppendTurn");
        }
        StableAction::AppendModeNote { mode, user_text } => {
            let m = MODES[(*mode as usize) % MODES.len()];
            messages.push(Message::assistant(Some(Content::text(String::from("ok"))), None, None));
            messages.push(Message::user(Content::text(format!("[smelt:mode] now in {m} mode."))));
            messages.push(Message::user(Content::text(user_text.clone())));
            let after = body(&messages, &tools, &cfg);
            assert_prefix_stable(&before, &after, prefix_msg_count, "AppendModeNote");
        }
        StableAction::ReorderTools => {
            // Reverse the input order before sorting — sort must produce
            // the same output bytes.
            let mut reordered_arb: Vec<ArbTool> =
                dedup_arb_tools_by_name(&input.tools).into_iter().rev().collect();
            // Dedup again post-reverse in case order affected which entry
            // was the "first" with a given name.
            reordered_arb = dedup_arb_tools_by_name(&reordered_arb);
            let mut reordered: Vec<ToolDefinition> =
                reordered_arb.iter().map(|t| t.build()).collect();
            sort_tools_for_cache_stability(&mut reordered);
            let after = body(&messages, &reordered, &cfg);
            // Whole body should match — no message changes, just tools.
            assert_prefix_stable(&before, &after, prefix_msg_count, "ReorderTools");
        }
        StableAction::NudgeTemperature => {
            // ModelConfig has no public mutator for temperature in the
            // current API; the property we want to assert is "non-prefix
            // params don't enter the cached prefix bytes". The cheap proxy:
            // rebuild with a *different* effort level (which sits in the
            // sampling params, not the cached prefix) and check stability.
            let after = fuzz_build_anthropic_body(
                &messages,
                &tools,
                "m",
                ReasoningEffort::Low,
                &cfg,
                &cache_on(),
            );
            assert_prefix_stable(&before, &after, prefix_msg_count, "NudgeTemperature");
        }
    }
}

fuzz_target!(|input: Input| {
    run(input);
});
