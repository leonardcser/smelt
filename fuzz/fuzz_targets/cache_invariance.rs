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
//! Stable actions covered here are defined in `smelt_fuzz::cache_common`
//! and shared with the OpenAI target so a new action lands in both
//! targets at once.
//!
//! Each iteration: build a baseline body, apply one stable action,
//! build the post-action body, strip `cache_control` markers from both,
//! assert byte equality of the parts that must stay cached. A
//! divergence means a real bug: tokens are being re-billed.

use arbitrary::Arbitrary;
use engine::provider::{fuzz_build_anthropic_body, CacheConfig, ToolDefinition};
use engine::ModelConfig;
use libfuzzer_sys::fuzz_target;
use protocol::{mode_change_note, Content, Message, ReasoningEffort};
use serde_json::Value;
use smelt_fuzz::cache_common::{ArbTool, StableAction, MODES};

#[derive(Debug, Arbitrary)]
struct Input {
    initial_system: String,
    initial_user_texts: Vec<String>,
    tools: Vec<ArbTool>,
    action: StableAction,
}

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

fn body(
    messages: &[Message],
    tools: &[ToolDefinition],
    cfg: &ModelConfig,
    effort: ReasoningEffort,
) -> Value {
    fuzz_build_anthropic_body(messages, tools, "m", effort, cfg, &cache_on())
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
    let empty = Vec::new();
    let bm = b
        .get("messages")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);
    let am = a
        .get("messages")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);
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
        return;
    }
    let tools = smelt_fuzz::cache_common::build_tools(&input.tools);
    let cfg = ModelConfig::default();

    let before = body(&messages, &tools, &cfg, ReasoningEffort::Off);
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
            let after = body(&messages, &tools, &cfg, ReasoningEffort::Off);
            assert_prefix_stable(&before, &after, prefix_msg_count, "AppendTurn");
        }
        StableAction::AppendModeNote { mode, user_text } => {
            let m = MODES[(*mode as usize) % MODES.len()];
            messages.push(Message::assistant(
                Some(Content::text(String::from("ok"))),
                None,
                None,
            ));
            messages.push(Message::user(Content::text(mode_change_note(m))));
            messages.push(Message::user(Content::text(user_text.clone())));
            let after = body(&messages, &tools, &cfg, ReasoningEffort::Off);
            assert_prefix_stable(&before, &after, prefix_msg_count, "AppendModeNote");
        }
        StableAction::ReorderTools => {
            let reordered = smelt_fuzz::cache_common::reorder_tools(&input.tools);
            let after = body(&messages, &reordered, &cfg, ReasoningEffort::Off);
            assert_prefix_stable(&before, &after, prefix_msg_count, "ReorderTools");
        }
        StableAction::NudgeReasoningEffort => {
            let after = body(&messages, &tools, &cfg, ReasoningEffort::Low);
            assert_prefix_stable(&before, &after, prefix_msg_count, "NudgeReasoningEffort");
        }
    }
}

fuzz_target!(|input: Input| {
    run(input);
});
