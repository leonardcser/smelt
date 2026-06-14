//! Static permission rule types, compilation, and pattern matching.

use protocol::AgentMode;
use protocol::Decision;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;

/// Custom decision function for a subpattern bucket. When present, `check_subcommand` delegates.
pub type SubpatternParserFn = dyn Fn(&RuleSet, &str, AgentMode) -> Decision + Send + Sync;

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct RawRuleSet {
    pub allow: Vec<String>,
    pub ask: Vec<String>,
    pub deny: Vec<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct RawModePerms {
    pub tools: RawRuleSet,
    #[serde(default)]
    pub subcommands: HashMap<String, RawRuleSet>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct RawPerms {
    pub default: RawModePerms,
    #[serde(flatten)]
    pub modes: HashMap<String, RawModePerms>,
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

#[derive(Debug, Default, Clone, Deserialize)]
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
pub struct ModeBehavior {
    pub default_decision: Decision,
    pub allow_subcommands_by_default: bool,
    pub ask_on_output_redirection: bool,
    pub read_only: bool,
}

impl Default for ModeBehavior {
    fn default() -> Self {
        Self {
            default_decision: Decision::Ask,
            allow_subcommands_by_default: false,
            ask_on_output_redirection: true,
            read_only: false,
        }
    }
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
    // Deny wins - inserted last so it overwrites allow/ask
    for name in &raw.deny {
        map.insert(name.clone(), Decision::Deny);
    }
    map
}

/// Per-tool, per-mode defaults declared at registration. Missing modes fall back to the global default.
#[derive(Debug, Default, Clone)]
pub struct ToolPermDefaults {
    pub modes: HashMap<String, Decision>,
}

impl ToolPermDefaults {
    pub fn for_mode(&self, mode: &AgentMode) -> Option<&Decision> {
        self.modes.get(mode.as_str())
    }
}

/// Coarse effect declared by a tool at registration time.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ToolEffectKind {
    PathRead,
    PathWrite,
    Network,
    UserInteraction,
    ProcessRead,
    ProcessControl,
    ConfigReload,
    #[default]
    Unknown,
}

/// Aggregated tool-declared defaults consumed by `Permissions::from_raw`.
#[derive(Default, Clone)]
pub struct ToolDefaults {
    pub tool_decisions: HashMap<String, ToolPermDefaults>,
    pub tool_effects: HashMap<String, ToolEffectKind>,
    pub subcommand_allow: HashMap<String, Vec<String>>,
    pub subpattern_parsers: HashMap<String, Arc<SubpatternParserFn>>,
}

impl std::fmt::Debug for ToolDefaults {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolDefaults")
            .field("tool_decisions", &self.tool_decisions)
            .field("tool_effects", &self.tool_effects)
            .field("subcommand_allow", &self.subcommand_allow)
            .field(
                "subpattern_parsers",
                &self.subpattern_parsers.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// Compile a raw subpattern bucket. Tool-declared `default_allow` is used when no user patterns
/// are configured; Yolo always falls back to `*`.
fn build_subcommand_ruleset(
    name: &str,
    raw: &RawRuleSet,
    mode_behavior: &ModeBehavior,
    tool_defaults: &ToolDefaults,
) -> RuleSet {
    let mut allow = compile_patterns(&raw.allow);
    if allow.is_empty() {
        if mode_behavior.allow_subcommands_by_default {
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
    mode: &AgentMode,
    mode_behavior: ModeBehavior,
    tool_defaults: &ToolDefaults,
) -> ModePerms {
    let mut tools = build_tool_map(&raw.tools);
    // Fill gaps where user config doesn't specify a decision.
    for (name, perms) in &tool_defaults.tool_decisions {
        if let Some(d) = perms.for_mode(mode) {
            tools.entry(name.clone()).or_insert_with(|| d.clone());
        }
    }

    let mut subcommands: HashMap<String, RuleSet> = HashMap::new();
    for (name, rs) in &raw.subcommands {
        subcommands.insert(
            name.clone(),
            build_subcommand_ruleset(name, rs, &mode_behavior, tool_defaults),
        );
    }
    // Insert default rulesets for tool-declared buckets the user didn't configure.
    for name in tool_defaults.subcommand_allow.keys() {
        subcommands.entry(name.clone()).or_insert_with(|| {
            build_subcommand_ruleset(name, &RawRuleSet::default(), &mode_behavior, tool_defaults)
        });
    }

    ModePerms { tools, subcommands }
}

pub(super) fn matches_rule(pat: &glob::Pattern, value: &str) -> bool {
    // Also match with a trailing space so "ls *" matches bare "ls" (no arguments).
    pat.matches(value) || pat.matches(&format!("{value} "))
}

pub(super) fn check_ruleset(ruleset: &RuleSet, value: &str) -> Decision {
    // Deny wins unconditionally.
    for pat in &ruleset.deny {
        if matches_rule(pat, value) {
            return Decision::Deny;
        }
    }

    // Most specific (longest) pattern wins; on tie, ask wins (safer default).
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
