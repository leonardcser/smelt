//! Rule-set types, compilation, mode construction, and pattern matching.
//!
//! This module owns the static permission rules loaded from config:
//! - `Decision`, `RuleSet`, `ModePerms`
//! - Raw deserialization types and merge helpers
//! - `DEFAULT_BASH_ALLOW` safe-read-only-command list
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

/// Default bash patterns that are allowed without explicit approval.
/// Used by both permissions checking and approval pattern suggestions.
pub const DEFAULT_BASH_ALLOW: &[&str] = &[
    // Directory listing & file search
    "ls *",
    "find *",
    "tree *",
    // Text viewing
    "cat *",
    "head *",
    "tail *",
    "less *",
    // Text search & processing (read-only)
    "grep *",
    "sort *",
    "uniq *",
    "wc *",
    "diff *",
    "tr *",
    "cut *",
    "jq *",
    // Path & file info
    "echo *",
    "pwd *",
    "which *",
    "dirname *",
    "basename *",
    "realpath *",
    "stat *",
    "file *",
    "test *",
    // Disk & system info
    "du *",
    "df *",
    "date *",
    "whoami *",
    // Binary inspection
    "sha256sum *",
    "md5sum *",
    "xxd *",
    "hexdump *",
    "strings *",
];

/// Mode defaults: which tool names get allow/ask/deny when neither
/// the config nor a runtime override speaks. Lives in core (not Lua) so
/// the engine has reasonable behaviour when no `init.lua` is loaded;
/// real product UX overrides this via `smelt.permissions.set_rules`.
fn install_tool_defaults(tools: &mut HashMap<String, Decision>, mode: AgentMode) {
    if mode == AgentMode::Yolo {
        // Yolo: everything not explicitly denied → Allow. The catch-all
        // also lives in `Permissions::check_tool` for unregistered names.
        for name in [
            "read_file",
            "edit_file",
            "write_file",
            "glob",
            "grep",
            "ask_user_question",
            "bash",
            "web_fetch",
            "web_search",
            "read_process_output",
            "stop_process",
        ] {
            tools.entry(name.to_string()).or_insert(Decision::Allow);
        }
        return;
    }
    tools
        .entry("read_file".to_string())
        .or_insert(Decision::Allow);
    let default_edit = if mode == AgentMode::Apply {
        Decision::Allow
    } else {
        Decision::Ask
    };
    tools.entry("edit_file".to_string()).or_insert(default_edit);
    let default_write = if mode == AgentMode::Apply {
        Decision::Allow
    } else {
        Decision::Ask
    };
    tools
        .entry("write_file".to_string())
        .or_insert(default_write);
    tools.entry("glob".to_string()).or_insert(Decision::Allow);
    tools.entry("grep".to_string()).or_insert(Decision::Allow);
    tools
        .entry("ask_user_question".to_string())
        .or_insert(Decision::Allow);
}

/// Compile a raw subpattern bucket into a `RuleSet`. `bucket_name` is
/// used for bucket-specific defaults: `bash` falls back to
/// `DEFAULT_BASH_ALLOW` in non-Yolo modes when no allow patterns are
/// configured; every bucket falls back to `*` in Yolo.
fn build_subcommand_ruleset(name: &str, raw: &RawRuleSet, mode: AgentMode) -> RuleSet {
    let mut allow = compile_patterns(&raw.allow);
    if allow.is_empty() {
        if mode == AgentMode::Yolo {
            allow = vec![glob::Pattern::new("*").unwrap()];
        } else if name == "bash" {
            allow = compile_patterns(
                &DEFAULT_BASH_ALLOW
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>(),
            );
        }
    }
    RuleSet {
        allow,
        ask: compile_patterns(&raw.ask),
        deny: compile_patterns(&raw.deny),
    }
}

pub(super) fn build_mode(raw: &RawModePerms, mode: AgentMode) -> ModePerms {
    let mut tools = build_tool_map(&raw.tools);
    install_tool_defaults(&mut tools, mode);

    let mut subcommands: HashMap<String, RuleSet> = HashMap::new();
    // Ensure bash always has a ruleset so `DEFAULT_BASH_ALLOW` engages
    // even when no `bash =` block is configured.
    if !raw.subcommands.contains_key("bash") {
        subcommands.insert(
            "bash".to_string(),
            build_subcommand_ruleset("bash", &RawRuleSet::default(), mode),
        );
    }
    for (name, rs) in &raw.subcommands {
        subcommands.insert(name.clone(), build_subcommand_ruleset(name, rs, mode));
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
