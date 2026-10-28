pub(crate) const HOST_INSTRUCTIONS: &str = include_str!("prompts/host.txt");

/// Compose host additions without inserting smelt's model-agent persona or tool
/// conventions into a native agent's preset. An explicit override replaces only
/// that preset; application invariants and registered additions remain in force.
pub fn build_native_system_prompt(
    override_prompt: Option<&str>,
    instructions: Option<&str>,
    skills: Option<&str>,
    fragments: &[String],
) -> protocol::NativeSystemPrompt {
    let mut sections = vec![
        "You are running inside smelt through the Claude Code agent. Use your native tools for coding, shell commands, search, questions, and planning. The smelt MCP server adds application capabilities and explicitly configured custom replacements. AskUserQuestion and ExitPlanMode are connected to smelt's native UI. Use smelt's workspace tools rather than a shell cd to change smelt's workspace binding. Claude Code owns native context management and native background tasks; smelt job IDs belong only to smelt-managed jobs.".to_owned(),
        HOST_INSTRUCTIONS.trim().to_owned(),
    ];
    for section in [instructions, skills]
        .into_iter()
        .flatten()
        .chain(fragments.iter().map(String::as_str))
    {
        if !section.trim().is_empty() {
            sections.push(section.to_owned());
        }
    }
    protocol::NativeSystemPrompt {
        override_prompt: override_prompt.map(str::to_owned),
        append: sections.join("\n\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_preset_and_explicit_override_keep_host_and_plugin_rules_separate() {
        for replacement in [None, Some("custom native persona")] {
            let prompt = build_native_system_prompt(
                replacement,
                Some("project rules"),
                Some("smelt-only skills"),
                &["plugin guidance".into()],
            );
            assert_eq!(prompt.override_prompt.as_deref(), replacement);
            for expected in [
                "project rules",
                "smelt-only skills",
                "plugin guidance",
                "Do not create a pull request, push a branch",
            ] {
                assert!(prompt.append.contains(expected));
            }
            assert!(!prompt.append.contains("Use dedicated tools over bash"));
            assert!(!prompt.append.contains("custom native persona"));
        }
    }
}
