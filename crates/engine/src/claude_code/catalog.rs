use protocol::{AgentMode, ToolDef};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};

/// One owner per native capability. Internal UI adapters stay callable by the
/// host without becoming a second model-visible question or planning toolkit.
#[derive(Default)]
pub(super) struct Catalog {
    exposed: HashSet<String>,
    visibility: protocol::ToolVisibility,
    pub(super) disabled_native: Vec<String>,
}

/// Native tools that are the same capability as a smelt tool. They share its
/// permission rules and transcript presentation.
const SHARED: &[(&str, &str)] = &[
    ("Read", "read_file"),
    ("Write", "write_file"),
    ("Edit", "edit_file"),
    ("MultiEdit", "edit_file"),
    ("NotebookEdit", "edit_notebook"),
    ("Bash", "bash"),
    ("Glob", "glob"),
    ("Grep", "grep"),
    ("WebFetch", "web_fetch"),
    ("WebSearch", "web_search"),
    ("AskUserQuestion", "ask_user_question"),
    ("ExitPlanMode", "present_plan"),
];

/// Native tools whose capability a smelt tool owns without sharing its shape.
const OWNED: &[(&str, &str)] = &[
    ("TaskOutput", "read_process_output"),
    ("TaskStop", "stop_process"),
];

fn native_equivalents(name: &str) -> Vec<&'static str> {
    SHARED
        .iter()
        .chain(OWNED)
        .filter(|(_, smelt)| *smelt == name)
        .map(|(native, _)| *native)
        .collect()
}

/// The smelt tool sharing a native tool's capability, with the call's
/// arguments renamed to that tool's parameters.
pub fn smelt_tool(
    native: &str,
    args: &HashMap<String, Value>,
) -> Option<(&'static str, HashMap<String, Value>)> {
    let (_, smelt) = SHARED.iter().find(|(name, _)| *name == native)?;
    let args = args
        .iter()
        .map(|(key, value)| {
            let key = match (native, key.as_str()) {
                ("Bash", "timeout") => "timeout_ms",
                ("Bash", "run_in_background") => "background",
                _ => key,
            };
            (key.to_owned(), value.clone())
        })
        .collect();
    Some((smelt, args))
}

impl Catalog {
    pub(super) fn new(
        tools: &[ToolDef],
        visibility: protocol::ToolVisibility,
    ) -> Result<Self, String> {
        let mut exposed = HashSet::new();
        let mut disabled = BTreeSet::new();
        for tool in tools {
            let equivalents = native_equivalents(&tool.name);
            let Some(policy) = &tool.claude_code else {
                if equivalents.is_empty() {
                    exposed.insert(tool.name.clone());
                }
                continue;
            };
            if !policy.enabled {
                if !policy.replaces.is_empty() {
                    return Err(format!(
                        "Hidden tool {} cannot replace native tools",
                        tool.name
                    ));
                }
                continue;
            }
            if equivalents
                .iter()
                .any(|native| !policy.replaces.iter().any(|name| name == native))
            {
                return Err(format!(
                    "Tool {} duplicates a native capability; claude_code.replaces must include {}",
                    tool.name,
                    equivalents.join(", ")
                ));
            }
            for native in &policy.replaces {
                if native.is_empty()
                    || !native
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                {
                    return Err(format!(
                        "Invalid native tool name in {}: {native}",
                        tool.name
                    ));
                }
                if !disabled.insert(native.clone()) {
                    return Err(format!("Multiple smelt tools replace native tool {native}"));
                }
            }
            exposed.insert(tool.name.clone());
        }
        Ok(Self {
            exposed,
            visibility,
            disabled_native: disabled.into_iter().collect(),
        })
    }

    pub(super) fn exposes(&self, tool: &ToolDef, mode: &AgentMode) -> bool {
        self.visibility.includes(tool.headless)
            && self.exposed.contains(&tool.name)
            && tool.modes.as_ref().is_none_or(|modes| modes.contains(mode))
    }

    pub(super) fn owns(&self, native: &str) -> bool {
        self.disabled_native.iter().any(|name| name == native)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn build_catalog(tools: &[ToolDef]) -> Result<Catalog, String> {
        Catalog::new(tools, protocol::ToolVisibility::Interactive)
    }

    fn tool(name: &str, policy: serde_json::Value) -> ToolDef {
        serde_json::from_value(
            json!({"name":name,"description":"test","parameters":{},"claude_code":policy}),
        )
        .unwrap()
    }

    #[test]
    fn defaults_are_native_first_without_hiding_custom_capabilities() {
        let tools = vec![
            tool("bash", json!(null)),
            tool("load_skill", json!(null)),
            tool("custom", json!(null)),
        ];
        let catalog = build_catalog(&tools).unwrap();
        assert!(!catalog.exposes(&tools[0], &AgentMode::normal()));
        assert!(catalog.exposes(&tools[1], &AgentMode::normal()));
        assert!(catalog.exposes(&tools[2], &AgentMode::normal()));
        assert!(catalog.disabled_native.is_empty());
    }

    #[test]
    fn headless_visibility_does_not_change_native_ownership() {
        let mut replacement = tool("ui_shell", json!({"replaces":["Bash"]}));
        replacement.headless = false;
        let mut complementary = tool("custom", json!(null));
        complementary.headless = true;
        let tools = [replacement, complementary];
        let catalog = Catalog::new(&tools, protocol::ToolVisibility::Headless).unwrap();
        assert!(catalog.owns("Bash"));
        assert!(!catalog.exposes(&tools[0], &AgentMode::normal()));
        assert!(catalog.exposes(&tools[1], &AgentMode::normal()));
        let mut conflict = tools[0].clone();
        conflict.name = "other_shell".into();
        conflict.headless = true;
        assert!(Catalog::new(
            &[tools[0].clone(), conflict],
            protocol::ToolVisibility::Headless
        )
        .is_err());
    }

    #[test]
    fn replacements_disable_native_tools_and_conflicts_fail_closed() {
        let replacement = tool("my_shell", json!({"enabled":true,"replaces":["Bash"]}));
        let catalog = build_catalog(std::slice::from_ref(&replacement)).unwrap();
        assert!(catalog.exposes(&replacement, &AgentMode::normal()));
        assert!(catalog.owns("Bash"));
        assert!(build_catalog(&[
            replacement,
            tool("other_shell", json!({"enabled":true,"replaces":["Bash"]}))
        ])
        .is_err());
        assert!(build_catalog(&[tool("bash", json!({"enabled":true}))]).is_err());
        assert!(build_catalog(&[tool("edit_file", json!({"replaces":["Edit"]}))]).is_err());
    }
}
