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
    pub effects: RawEffectRules,
    #[serde(default)]
    pub patterns: HashMap<String, RawRuleSet>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct RawEffectRules {
    pub read: Option<Decision>,
    pub write: Option<Decision>,
    pub network: Option<Decision>,
    pub process: Option<Decision>,
    pub config: Option<Decision>,
    pub user: Option<Decision>,
    pub other: Option<Decision>,
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

fn merge_effect_rules(default: &RawEffectRules, mode: &RawEffectRules) -> RawEffectRules {
    RawEffectRules {
        read: mode.read.clone().or_else(|| default.read.clone()),
        write: mode.write.clone().or_else(|| default.write.clone()),
        network: mode.network.clone().or_else(|| default.network.clone()),
        process: mode.process.clone().or_else(|| default.process.clone()),
        config: mode.config.clone().or_else(|| default.config.clone()),
        user: mode.user.clone().or_else(|| default.user.clone()),
        other: mode.other.clone().or_else(|| default.other.clone()),
    }
}

pub(super) fn merge_mode(default: &RawModePerms, mode: &RawModePerms) -> RawModePerms {
    let mut patterns: HashMap<String, RawRuleSet> = HashMap::new();
    let keys: std::collections::HashSet<&String> = default
        .patterns
        .keys()
        .chain(mode.patterns.keys())
        .collect();
    for key in keys {
        let d = default.patterns.get(key);
        let m = mode.patterns.get(key);
        let merged = match (d, m) {
            (Some(d), Some(m)) => merge_ruleset(d, m),
            (Some(d), None) => merge_ruleset(d, &RawRuleSet::default()),
            (None, Some(m)) => merge_ruleset(&RawRuleSet::default(), m),
            (None, None) => RawRuleSet::default(),
        };
        patterns.insert(key.clone(), merged);
    }
    RawModePerms {
        tools: merge_ruleset(&default.tools, &mode.tools),
        effects: merge_effect_rules(&default.effects, &mode.effects),
        patterns,
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
}

impl Default for ModeBehavior {
    fn default() -> Self {
        Self {
            default_decision: Decision::Ask,
            allow_subcommands_by_default: false,
            ask_on_output_redirection: true,
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct ModePerms {
    pub(super) tools: HashMap<String, Decision>,
    pub(super) effects: EffectPerms,
    pub(super) patterns: HashMap<String, RuleSet>,
}

#[derive(Debug, Default, Clone)]
pub(super) struct EffectPerms {
    pub(super) read: Option<Decision>,
    pub(super) write: Option<Decision>,
    pub(super) network: Option<Decision>,
    pub(super) process: Option<Decision>,
    pub(super) config: Option<Decision>,
    pub(super) user: Option<Decision>,
    pub(super) other: Option<Decision>,
}

impl From<RawEffectRules> for EffectPerms {
    fn from(raw: RawEffectRules) -> Self {
        Self {
            read: raw.read,
            write: raw.write,
            network: raw.network,
            process: raw.process,
            config: raw.config,
            user: raw.user,
            other: raw.other,
        }
    }
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
    Read,
    Write,
    Network,
    User,
    Process,
    Config,
    #[default]
    Other,
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
    _mode: &AgentMode,
    mode_behavior: ModeBehavior,
    tool_defaults: &ToolDefaults,
) -> ModePerms {
    let tools = build_tool_map(&raw.tools);

    let mut patterns: HashMap<String, RuleSet> = HashMap::new();
    for (name, rs) in &raw.patterns {
        patterns.insert(
            name.clone(),
            build_subcommand_ruleset(name, rs, &mode_behavior, tool_defaults),
        );
    }
    // Insert default rulesets for tool-declared buckets the user didn't configure.
    for name in tool_defaults.subcommand_allow.keys() {
        patterns.entry(name.clone()).or_insert_with(|| {
            build_subcommand_ruleset(name, &RawRuleSet::default(), &mode_behavior, tool_defaults)
        });
    }

    ModePerms {
        tools,
        effects: raw.effects.clone().into(),
        patterns,
    }
}

pub(super) fn matches_rule(pat: &glob::Pattern, value: &str) -> bool {
    // Also match with a trailing space so "ls *" matches bare "ls" (no arguments).
    pat.matches(value) || pat.matches(&format!("{value} "))
}

pub(super) fn check_ruleset_match(ruleset: &RuleSet, value: &str) -> Option<Decision> {
    // Deny wins unconditionally.
    for pat in &ruleset.deny {
        if matches_rule(pat, value) {
            return Some(Decision::Deny);
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
        (Some(a), Some(k)) => Some(if k >= a {
            Decision::Ask
        } else {
            Decision::Allow
        }),
        (Some(_), None) => Some(Decision::Allow),
        (None, Some(_)) => Some(Decision::Ask),
        (None, None) => None,
    }
}

pub(super) fn check_ruleset(ruleset: &RuleSet, value: &str) -> Decision {
    check_ruleset_match(ruleset, value).unwrap_or(Decision::Ask)
}
