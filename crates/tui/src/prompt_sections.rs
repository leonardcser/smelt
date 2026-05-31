use protocol::AgentMode;

/// Ordered collection of named system prompt sections.
///
/// The TUI builds these from app state and sends the assembled result to the
/// engine. Lua plugins can add, remove, or replace sections by name.
#[derive(Clone, Default)]
pub(crate) struct PromptSections {
    sections: Vec<(String, String)>,
}

impl PromptSections {
    /// Insert or replace a section. If a section with this name exists,
    /// it is replaced in-place. Otherwise it is appended at the end.
    pub(crate) fn set(&mut self, name: &str, content: String) {
        if let Some(entry) = self.sections.iter_mut().find(|(n, _)| n == name) {
            entry.1 = content;
        } else {
            self.sections.push((name.to_string(), content));
        }
    }

    /// Remove a section by name. No-op if the section doesn't exist.
    pub(crate) fn remove(&mut self, name: &str) {
        self.sections.retain(|(n, _)| n != name);
    }

    /// Concatenate all non-empty sections with double newlines.
    pub(crate) fn assemble(&self) -> String {
        let mut out = String::new();
        for (_, content) in &self.sections {
            let trimmed = content.trim();
            if trimmed.is_empty() {
                continue;
            }
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(trimmed);
        }
        out
    }
}

fn base_section(cwd: &std::path::Path) -> String {
    format!(
        "You are an expert coding agent running in the user's terminal. You help with \
         software engineering tasks: reading code, finding bugs, explaining patterns, and \
         implementing changes.\n\
         \n\
         Working directory: {cwd}\n\
         \n\
         # Tools\n\
         - Use dedicated tools over bash: read_file instead of cat, edit_file instead of sed, \
         glob instead of find, grep instead of grep/rg.\n\
         - Always read a file with read_file before editing it.\n\
         - **Always use edit_file for modifying existing files.** Only use write_file to create \
         new files. Never use write_file to overwrite an existing file — use edit_file instead, \
         even for large changes. If you need to replace most of a file, make multiple edit_file \
         calls.\n\
         - To move or rename files, use `mv` in bash. Do not delete and recreate them.\n\
         - Call multiple tools in parallel when there are no dependencies between them.\n\
         \n\
         # Code\n\
         - Elegant code is simple. No over-abstraction or over-engineering. Easy to test, debug, \
         and delete.\n\
         - Prefer concrete types over premature interfaces. Start in one file; split only when \
         unwieldy.\n\
         - Match naming to the existing codebase. Descriptive names for important things, short \
         names for locals and loops.\n\
         - Follow idiomatic error handling for each language.\n\
         - Every change should read as if the new implementation was always there. No traces of \
         what came before — no shims, no \"changed from X to Y\" comments, no commented-out \
         blocks. Comments describe what the code does, not what it used to be.\n\
         - Use the package manager's install command for dependencies. Never manually edit \
         dependency files.\n\
         - Never introduce code that exposes or logs secrets and keys. Never commit secrets or keys.\n\
         \n\
         # Approach\n\
         - Think before you act — understand the problem before reaching for tools or writing code.\n\
         - Read relevant files before making suggestions. Use glob and grep to search efficiently.\n\
         - Start debugging with the simplest root cause hypothesis. Diagnose first, fix once. \
         If a fix doesn't work, re-examine assumptions rather than guessing again.\n\
         - Never create files unless absolutely necessary. Prefer editing existing files.",
        cwd = cwd.display(),
    )
}

fn interactive_behavior() -> &'static str {
    "# Behavior\n\
     You and the user are collaborators — you bring your full intellectual weight, ask sharp \
     questions, and surface options they might not have considered.\n\
     - Be concise and direct. Keep responses short and summarized — expand only when asked \
     for more detail.\n\
     - When asked to solve a problem, present multiple approaches with trade-offs. Include bold \
     options — what would a rewrite from scratch look like? Recommend one approach and explain why.\n\
     - Proactively ask for feedback and clarification — align early rather than course-correct later.\n\
     - When modifying files, explain what you're changing and why.\n\
     - No emojis unless the user asks for them.\n\
     - No unnecessary praise, superlatives, or emotional validation. Prioritize technical accuracy \
     — disagree when necessary.\n\
     - When referencing code, use the pattern `file_path:line_number`.\n\
     - Output is rendered as markdown in a monospace terminal."
}

fn autonomous_behavior() -> &'static str {
    "# Behavior\n\
     You are running autonomously without a human in the loop.\n\
     - Make your best judgment and proceed without asking questions.\n\
     - Pick the best approach and implement it immediately. Do not present alternatives unless \
     uncertain.\n\
     - Do not narrate or explain your changes. Just make them."
}

/// Build the default prompt sections for a given mode and app state.
pub(crate) fn build_defaults(
    cwd: &std::path::Path,
    mode: AgentMode,
    interactive: bool,
    skill_section: Option<&str>,
    extra_instructions: Option<&str>,
) -> PromptSections {
    let mut ps = PromptSections::default();

    ps.set("base", base_section(cwd));

    ps.set(
        "behavior",
        if interactive {
            interactive_behavior().to_string()
        } else {
            autonomous_behavior().to_string()
        },
    );

    let _ = mode; // mode no longer feeds the cacheable system prompt
    if let Some(skills) = skill_section {
        if !skills.is_empty() {
            ps.set("skills", skills.to_string());
        }
    }

    if let Some(instructions) = extra_instructions {
        if !instructions.is_empty() {
            ps.set("instructions", instructions.to_string());
        }
    }

    ps
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // ── PromptSections ───────────────────────────────────────────────────

    #[test]
    fn set_appends_new_section_at_the_end() {
        let mut ps = PromptSections::default();
        ps.set("a", "first".into());
        ps.set("b", "second".into());
        assert_eq!(ps.assemble(), "first\n\nsecond");
    }

    #[test]
    fn set_replaces_existing_section_in_place_preserving_order() {
        let mut ps = PromptSections::default();
        ps.set("a", "first".into());
        ps.set("b", "second".into());
        ps.set("a", "replaced".into());
        // Section `a` keeps its original position; only the content changes.
        assert_eq!(ps.assemble(), "replaced\n\nsecond");
    }

    #[test]
    fn remove_drops_the_named_section() {
        let mut ps = PromptSections::default();
        ps.set("a", "first".into());
        ps.set("b", "second".into());
        ps.remove("a");
        assert_eq!(ps.assemble(), "second");
    }

    #[test]
    fn remove_is_a_noop_for_unknown_names() {
        let mut ps = PromptSections::default();
        ps.set("a", "first".into());
        ps.remove("nope");
        assert_eq!(ps.assemble(), "first");
    }

    #[test]
    fn assemble_skips_sections_that_trim_to_empty() {
        let mut ps = PromptSections::default();
        ps.set("a", "first".into());
        ps.set("blank", "   \n\t  ".into());
        ps.set("b", "second".into());
        assert_eq!(ps.assemble(), "first\n\nsecond");
    }

    #[test]
    fn assemble_trims_each_section_individually() {
        let mut ps = PromptSections::default();
        ps.set("a", "  first  \n".into());
        ps.set("b", "\n\nsecond\n\n".into());
        assert_eq!(ps.assemble(), "first\n\nsecond");
    }

    #[test]
    fn assemble_of_empty_collection_is_empty_string() {
        let ps = PromptSections::default();
        assert_eq!(ps.assemble(), "");
    }

    // ── build_defaults ────────────────────────────────────────────────────

    fn names(ps: &PromptSections) -> Vec<&str> {
        ps.sections.iter().map(|(n, _)| n.as_str()).collect()
    }

    fn mode(name: &str) -> AgentMode {
        AgentMode::parse(name).unwrap()
    }

    #[test]
    fn build_defaults_includes_base_and_behavior_for_normal_mode() {
        let ps = build_defaults(Path::new("/work"), mode("normal"), true, None, None);
        assert_eq!(names(&ps), vec!["base", "behavior"]);
    }

    #[test]
    fn build_defaults_picks_interactive_vs_autonomous_behavior_section() {
        let interactive = build_defaults(Path::new("/w"), mode("normal"), true, None, None);
        let autonomous = build_defaults(Path::new("/w"), mode("normal"), false, None, None);
        let i_body = &interactive
            .sections
            .iter()
            .find(|(n, _)| n == "behavior")
            .unwrap()
            .1;
        let a_body = &autonomous
            .sections
            .iter()
            .find(|(n, _)| n == "behavior")
            .unwrap()
            .1;
        assert!(i_body.contains("collaborators"));
        assert!(a_body.contains("autonomously"));
        assert_ne!(i_body, a_body);
    }

    #[test]
    fn build_defaults_is_byte_stable_across_modes() {
        // The base prompt must not change with mode; mode-specific
        // behavior is communicated via a runtime message instead.
        let cwd = Path::new("/w");
        let plan = build_defaults(cwd, mode("plan"), true, None, None).assemble();
        let apply = build_defaults(cwd, mode("apply"), true, None, None).assemble();
        let yolo = build_defaults(cwd, mode("yolo"), true, None, None).assemble();
        let normal = build_defaults(cwd, mode("normal"), true, None, None).assemble();
        assert_eq!(plan, apply);
        assert_eq!(apply, yolo);
        assert_eq!(yolo, normal);
    }

    #[test]
    fn build_defaults_appends_skill_and_instruction_sections_when_provided() {
        let ps = build_defaults(
            Path::new("/w"),
            mode("normal"),
            true,
            Some("# Skills\nfoo"),
            Some("Project rules: be terse."),
        );
        let n = names(&ps);
        assert!(n.contains(&"skills"));
        assert!(n.contains(&"instructions"));
    }

    #[test]
    fn build_defaults_skips_empty_skill_and_instruction_strings() {
        let ps = build_defaults(Path::new("/w"), mode("normal"), true, Some(""), Some(""));
        let n = names(&ps);
        assert!(!n.contains(&"skills"));
        assert!(!n.contains(&"instructions"));
    }

    #[test]
    fn build_defaults_base_section_embeds_the_cwd() {
        let ps = build_defaults(Path::new("/some/where"), mode("normal"), true, None, None);
        let base = &ps.sections.iter().find(|(n, _)| n == "base").unwrap().1;
        assert!(base.contains("/some/where"), "got: {base}");
    }
}
