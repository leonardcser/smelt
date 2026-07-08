//! Shared building blocks for the prompt-cache invariance fuzz targets.
//!
//! Anthropic and OpenAI cache-key invariants are different in detail
//! (`cache_control` markers vs `prompt_cache_key`, `system` vs
//! `instructions`, message vs input arrays), but the random fixture
//! shape is identical: a small tool palette, a deduped history of user /
//! assistant turns, and a `StableAction` describing one mutation that
//! must *not* invalidate the cache prefix. This module owns the shared
//! fixture so adding a new stable action - say "reorder system prompts"
//! - happens in exactly one place and lands in both targets.

use arbitrary::Arbitrary;
use smelt_provider::{sort_tools_for_cache_stability, FunctionSchema, ToolDefinition};
use protocol::AgentMode;

/// Bounded tool name palette. Index-modulated so two histories can
/// collide on the same name and exercise dedup behaviour.
pub const TOOL_NAMES: &[&str] = &[
    "read", "write", "edit", "grep", "glob", "ls", "bash", "fetch", "spawn",
];

/// Mode names the synthetic `[smelt:mode]` note iterates through.
pub const MODE_NAMES: &[&str] = &["normal", "plan", "apply", "yolo"];

pub fn mode_at(index: usize) -> AgentMode {
    AgentMode::parse(MODE_NAMES[index % MODE_NAMES.len()]).unwrap()
}

/// One tool definition, hand-rolled rather than `derive(Arbitrary)` so
/// the parameters schema stays valid JSON (random `Value`s would produce
/// mostly garbage that doesn't exercise the cache machinery).
#[derive(Debug, Clone, Arbitrary)]
pub struct ArbTool {
    pub name_idx: u8,
    pub description: String,
}

impl ArbTool {
    pub fn build(&self) -> ToolDefinition {
        let name = TOOL_NAMES[(self.name_idx as usize) % TOOL_NAMES.len()].to_string();
        ToolDefinition::new(FunctionSchema {
            name,
            description: self.description.clone(),
            parameters: serde_json::json!({"type": "object"}),
        })
    }
}

/// One mutation that must NOT invalidate the cache prefix.
#[derive(Debug, Clone, Arbitrary)]
pub enum StableAction {
    /// Append `assistant_text, user_text` - the canonical follow-up turn.
    AppendTurn {
        assistant_text: String,
        user_text: String,
    },
    /// Append a `[smelt:mode]` synthetic note + a regular user turn -
    /// mirrors `/mode` switching, which lands in the message stream
    /// rather than the system prompt.
    AppendModeNote { mode: u8, user_text: String },
    /// Reorder tools - `sort_tools_for_cache_stability` must produce the
    /// same output regardless of registration order.
    ReorderTools,
    /// Toggle reasoning effort - sits in sampling params, outside the
    /// cached prefix.
    NudgeReasoningEffort,
}

/// Dedup `ArbTool`s by their resolved name and cap at six entries.
/// Cap keeps each iteration cheap; dedup matches what
/// `sort_tools_for_cache_stability` does internally (a duplicate name
/// would be a real bug, but exercising it isn't this target's job).
pub fn dedup_arb_tools_by_name(arb: &[ArbTool]) -> Vec<ArbTool> {
    let mut seen = std::collections::HashSet::new();
    arb.iter()
        .take(6)
        .filter(|t| seen.insert(t.name_idx as usize % TOOL_NAMES.len()))
        .cloned()
        .collect()
}

/// Build the tool list the same way both targets do: dedup + canonical
/// sort. Targets call this once for the baseline body.
pub fn build_tools(arb: &[ArbTool]) -> Vec<ToolDefinition> {
    let mut tools: Vec<ToolDefinition> = dedup_arb_tools_by_name(arb)
        .iter()
        .map(|t| t.build())
        .collect();
    sort_tools_for_cache_stability(&mut tools);
    tools
}

/// Reverse the input order, dedup again, then canonical sort. This is
/// the "did the sort produce stable output" probe - the input order
/// flipped, the sorted output must be byte-identical to `build_tools`.
pub fn reorder_tools(arb: &[ArbTool]) -> Vec<ToolDefinition> {
    let mut reordered: Vec<ArbTool> = dedup_arb_tools_by_name(arb).into_iter().rev().collect();
    reordered = dedup_arb_tools_by_name(&reordered);
    let mut out: Vec<ToolDefinition> = reordered.iter().map(|t| t.build()).collect();
    sort_tools_for_cache_stability(&mut out);
    out
}
