//! Rule-set types, compilation, mode construction, and pattern matching.
//!
//! This module owns the static permission rules loaded from config:
//! - `Decision`, `RuleSet`, `ModePerms`
//! - Raw deserialization types and merge helpers
//! - `ToolDefaults` — Lua-declared per-tool defaults (decisions per
//!   mode + subpattern allow lists), supplied by tool registration
//! - `build_mode` (materializes a `ModePerms` for one AgentMode)
//! - `check_ruleset` (the core pattern-matching decision)

use protocol::AgentMode;
use protocol::Decision;
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct RawRuleSet {
    pub allow: Vec<String>,
    pub ask: Vec<String>,
    pub deny: Vec<String>,
}

/// Per-mode permission rules. `tools` is the tool-name decision bucket;
/// `subcommands` is keyed by tool name (`bash`, `web_fetch`, `mcp`,
/// or any tool that registers one) and carries pattern rulesets the
/// tool's `decide` callback consults.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct RawModePerms {
    pub tools: RawRuleSet,
    #[serde(default)]
    pub subcommands: HashMap<String, RawRuleSet>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct RawPerms {
    pub default: RawModePerms,
    pub normal: RawModePerms,
    pub plan: RawModePerms,
    pub apply: RawModePerms,
    pub yolo: RawModePerms,
}

fn merge_ruleset(default: &RawRuleSet, mode: &RawRuleSet) -> RawRuleSet {
    RawRuleSet {
        allow: default.allow.iter().chain(&mode.allow).cloned().collect(),
        ask: default.ask.iter().chain(&mode.ask).cloned().collect(),
        deny: default.deny.iter().chain(&mode.deny).cloned().collect(),
    }
}

pub(super) fn merge_mode(default: &RawModePerms, mode: &RawModePerms) -> RawModePerms {
    let mut subcommands: HashMap<String, RawRuleSet> = HashMap::new();
    let keys: std::collections::HashSet<&String> = default
        .subcommands
        .keys()
        .chain(mode.subcommands.keys())
        .collect();
    for key in keys {
        let d = default.subcommands.get(key);
        let m = mode.subcommands.get(key);
        let merged = match (d, m) {
            (Some(d), Some(m)) => merge_ruleset(d, m),
            (Some(d), None) => merge_ruleset(d, &RawRuleSet::default()),
            (None, Some(m)) => merge_ruleset(&RawRuleSet::default(), m),
            (None, None) => RawRuleSet::default(),
        };
        subcommands.insert(key.clone(), merged);
    }
    RawModePerms {
        tools: merge_ruleset(&default.tools, &mode.tools),
        subcommands,
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct RawConfig {
    pub(super) permissions: RawPerms,
}

#[derive(Debug, Clone)]
pub struct RuleSet {
    pub(super) allow: Vec<glob::Pattern>,
    pub(super) ask: Vec<glob::Pattern>,
    pub(super) deny: Vec<glob::Pattern>,
}

#[derive(Debug, Clone)]
pub(super) struct ModePerms {
    pub(super) tools: HashMap<String, Decision>,
    pub(super) subcommands: HashMap<String, RuleSet>,
}

pub(super) fn compile_patterns(raw: &[String]) -> Vec<glob::Pattern> {
    raw.iter()
        .filter_map(|s| glob::Pattern::new(s).ok())
        .collect()
}

fn build_tool_map(raw: &RawRuleSet) -> HashMap<String, Decision> {
    let mut map = HashMap::new();
    for name in &raw.allow {
        map.insert(name.clone(), Decision::Allow);
    }
    for name in &raw.ask {
        map.insert(name.clone(), Decision::Ask);
    }
    // Deny wins — inserted last so it overwrites allow/ask
    for name in &raw.deny {
        map.insert(name.clone(), Decision::Deny);
    }
    map
}

/// Per-tool, per-mode default decisions declared by the tool's Lua
/// registration table (`permission_defaults = { normal = "ask", ... }`).
/// `None` means the tool didn't speak for that mode and the global
/// fallback in `Permissions::check_tool` (Yolo→Allow, else Ask) takes
/// over.
#[derive(Debug, Default, Clone)]
pub struct ToolPermDefaults {
    pub normal: Option<Decision>,
    pub plan: Option<Decision>,
    pub apply: Option<Decision>,
    pub yolo: Option<Decision>,
}

impl ToolPermDefaults {
    pub fn for_mode(&self, mode: AgentMode) -> Option<&Decision> {
        match mode {
            AgentMode::Normal => self.normal.as_ref(),
            AgentMode::Plan => self.plan.as_ref(),
            AgentMode::Apply => self.apply.as_ref(),
            AgentMode::Yolo => self.yolo.as_ref(),
        }
    }
}

/// Aggregated tool-declared defaults consumed by `Permissions::from_raw`.
/// `tool_decisions` keys are tool names; `subcommand_allow` keys are
/// subpattern bucket names (= tool names) and values are pattern lists
/// used as the bucket's allow fallback when neither user config nor
/// Yolo's `*` catch-all supplies one.
#[derive(Debug, Default, Clone)]
pub struct ToolDefaults {
    pub tool_decisions: HashMap<String, ToolPermDefaults>,
    pub subcommand_allow: HashMap<String, Vec<String>>,
}

/// Compile a raw subpattern bucket into a `RuleSet`. Tools that
/// declared `default_allow` via Lua registration (e.g. bash's safe
/// read-only prefix list) get those patterns as the allow fallback
/// when no user-configured allow patterns are present; every bucket
/// falls back to `*` in Yolo.
fn build_subcommand_ruleset(
    name: &str,
    raw: &RawRuleSet,
    mode: AgentMode,
    tool_defaults: &ToolDefaults,
) -> RuleSet {
    let mut allow = compile_patterns(&raw.allow);
    if allow.is_empty() {
        if mode == AgentMode::Yolo {
            allow = vec![glob::Pattern::new("*").unwrap()];
        } else if let Some(default_allow) = tool_defaults.subcommand_allow.get(name) {
            allow = compile_patterns(default_allow);
        }
    }
    RuleSet {
        allow,
        ask: compile_patterns(&raw.ask),
        deny: compile_patterns(&raw.deny),
    }
}

pub(super) fn build_mode(
    raw: &RawModePerms,
    mode: AgentMode,
    tool_defaults: &ToolDefaults,
) -> ModePerms {
    let mut tools = build_tool_map(&raw.tools);
    // Layer in tool-declared per-mode defaults; only fill gaps where
    // the user config doesn't already speak for the tool.
    for (name, perms) in &tool_defaults.tool_decisions {
        if let Some(d) = perms.for_mode(mode) {
            tools.entry(name.clone()).or_insert_with(|| d.clone());
        }
    }

    let mut subcommands: HashMap<String, RuleSet> = HashMap::new();
    for (name, rs) in &raw.subcommands {
        subcommands.insert(
            name.clone(),
            build_subcommand_ruleset(name, rs, mode, tool_defaults),
        );
    }
    // Buckets a tool declared `default_allow` for but the user didn't
    // configure: insert a default ruleset so the allow-fallback engages.
    for name in tool_defaults.subcommand_allow.keys() {
        subcommands.entry(name.clone()).or_insert_with(|| {
            build_subcommand_ruleset(name, &RawRuleSet::default(), mode, tool_defaults)
        });
    }

    ModePerms { tools, subcommands }
}

fn matches_rule(pat: &glob::Pattern, value: &str) -> bool {
    // Match both the value as-is and with a trailing space to handle
    // patterns like "ls *" matching bare "ls" (no arguments).
    pat.matches(value) || pat.matches(&format!("{value} "))
}

pub(super) fn check_ruleset(ruleset: &RuleSet, value: &str) -> Decision {
    // Deny always wins — checked first regardless of specificity.
    for pat in &ruleset.deny {
        if matches_rule(pat, value) {
            return Decision::Deny;
        }
    }

    // Among allow and ask, the most specific (longest pattern) wins.
    // On tie, ask wins (safer default). Pattern length is a heuristic for
    // specificity — works well for typical patterns like "git *" vs "git push *".
    let mut best_allow: Option<usize> = None;
    let mut best_ask: Option<usize> = None;

    for pat in &ruleset.allow {
        if matches_rule(pat, value) {
            let len = pat.as_str().len();
            if best_allow.is_none_or(|prev| len > prev) {
                best_allow = Some(len);
            }
        }
    }
    for pat in &ruleset.ask {
        if matches_rule(pat, value) {
            let len = pat.as_str().len();
            if best_ask.is_none_or(|prev| len > prev) {
                best_ask = Some(len);
            }
        }
    }

    match (best_allow, best_ask) {
        (Some(a), Some(k)) => {
            if k >= a {
                Decision::Ask
            } else {
                Decision::Allow
            }
        }
        (Some(_), None) => Decision::Allow,
        (None, Some(_)) => Decision::Ask,
        (None, None) => Decision::Ask,
    }
}
