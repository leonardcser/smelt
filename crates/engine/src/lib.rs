mod agent;
pub mod auth;
pub mod clock;
pub mod env;
pub mod host;
pub mod image;
pub mod log;
pub mod opener;

pub mod paths;

pub mod provider;
pub mod redact;
mod request_log;
pub(crate) mod result_dedup;
pub(crate) mod skills;
pub mod tools;
pub(crate) mod trim;

pub use host::{HostCall, HostRequestDecision};
use protocol::{EngineEvent, UiCommand};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

pub use paths::{config_dir, data_dir, home_dir, state_dir};

/// Re-export so non-engine crates (the TUI) can store an HTTP client
/// without depending on `reqwest` directly.
pub use reqwest::Client as HttpClient;

pub use provider::EngineProvider;
pub use skills::SkillLoader;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemPromptBehavior {
    Interactive,
    Autonomous,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SystemPromptCapabilities {
    pub tool_calling: bool,
}

impl SystemPromptCapabilities {
    pub fn from_tool_calling(tool_calling: bool) -> Self {
        Self { tool_calling }
    }
}

struct PromptContext<'a> {
    behavior: SystemPromptBehavior,
    capabilities: SystemPromptCapabilities,
    skills_section: Option<&'a str>,
    extra_instructions: Option<&'a str>,
}

pub fn build_system_prompt(
    behavior: SystemPromptBehavior,
    capabilities: SystemPromptCapabilities,
    extra_instructions: Option<&str>,
    skill_section: Option<&str>,
) -> String {
    let ctx = PromptContext {
        behavior,
        capabilities,
        skills_section: skill_section,
        extra_instructions,
    };
    render_system_prompt(&ctx)
}

pub fn assemble_system_prompt(
    system_prompt_override: Option<&str>,
    behavior: SystemPromptBehavior,
    capabilities: SystemPromptCapabilities,
    extra_instructions: Option<&str>,
    skill_section: Option<&str>,
) -> String {
    system_prompt_override
        .map(str::to_owned)
        .unwrap_or_else(|| {
            build_system_prompt(behavior, capabilities, extra_instructions, skill_section)
        })
}

fn render_system_prompt(ctx: &PromptContext<'_>) -> String {
    let rendered = render_builtin_prompt_template(include_str!("prompts/system.txt"), ctx);

    let mut result = collapse_blank_lines(&rendered);
    for section in [ctx.skills_section, ctx.extra_instructions]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|section| !section.is_empty())
    {
        if !result.is_empty() {
            result.push_str("\n\n");
        }
        result.push_str(section);
    }
    result
}

fn render_builtin_prompt_template(template: &str, ctx: &PromptContext<'_>) -> String {
    struct Frame {
        condition: bool,
    }

    let mut out = String::with_capacity(template.len());
    let mut stack: Vec<Frame> = Vec::new();

    for line in template.lines() {
        let trimmed = line.trim();
        match trimmed {
            "{% if tools_enabled %}" => {
                stack.push(Frame {
                    condition: ctx.capabilities.tool_calling,
                });
            }
            "{% if behavior == \"autonomous\" %}" => {
                stack.push(Frame {
                    condition: ctx.behavior == SystemPromptBehavior::Autonomous,
                });
            }
            "{% else %}" => {
                if let Some(frame) = stack.last_mut() {
                    frame.condition = !frame.condition;
                }
            }
            "{% endif %}" => {
                stack.pop();
            }
            _ if stack.iter().all(|frame| frame.condition) => {
                out.push_str(line);
                out.push('\n');
            }
            _ => {}
        }
    }

    out
}

fn collapse_blank_lines(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut blank_count = 0u32;
    for line in text.lines() {
        if line.trim().is_empty() {
            blank_count += 1;
            if blank_count <= 1 {
                result.push('\n');
            }
        } else {
            blank_count = 0;
            result.push_str(line);
            result.push('\n');
        }
    }
    result.trim().to_string()
}

#[cfg(test)]
mod system_prompt_tests {
    use super::*;

    fn prompt(behavior: SystemPromptBehavior, tool_calling: bool) -> String {
        build_system_prompt(
            behavior,
            SystemPromptCapabilities::from_tool_calling(tool_calling),
            Some("Extra instructions."),
            Some("# Skills\nLoaded skill."),
        )
    }

    #[test]
    fn interactive_prompt_renders_collaborator_behavior_and_tools() {
        let rendered = prompt(SystemPromptBehavior::Interactive, true);
        assert!(rendered.contains("# Tools"));
        assert!(rendered.contains("You and the user are collaborators"));
        assert!(!rendered.contains("You are running autonomously"));
        assert!(rendered.contains("# Skills\nLoaded skill."));
        assert!(rendered.ends_with("Extra instructions."));
        assert!(!rendered.contains("{%"));
    }

    #[test]
    fn autonomous_prompt_can_omit_tools() {
        let rendered = prompt(SystemPromptBehavior::Autonomous, false);
        assert!(!rendered.contains("# Tools"));
        assert!(rendered.contains("You are running autonomously"));
        assert!(!rendered.contains("You and the user are collaborators"));
        assert!(!rendered.contains("{%"));
    }
}

pub struct EngineConfig {
    pub instructions: Option<String>,
    /// When set, replaces the built-in system prompt template entirely.
    pub system_prompt_override: Option<String>,
    pub system_prompt_behavior: SystemPromptBehavior,
    pub cwd: PathBuf,
    /// Pre-rendered "# Skills" block injected into the system prompt.
    /// Built once on startup from the [`SkillLoader`] and refreshed through
    /// [`protocol::UiCommand::UpdateAgentProjectContext`]. The loader itself
    /// lives on `Core::skills` for tool execution.
    pub skill_section: Option<String>,
    /// Source of monotonic + wall-clock time. Production uses
    /// [`clock::RealClock`]; deterministic-simulation harnesses inject a
    /// [`clock::VirtualClock`] so scenarios can replay against advanced time.
    pub clock: Arc<dyn clock::Clock>,
}

impl EngineConfig {
    pub fn new(cwd: PathBuf, clock: Arc<dyn clock::Clock>) -> Self {
        Self {
            instructions: None,
            system_prompt_override: None,
            system_prompt_behavior: SystemPromptBehavior::Interactive,
            cwd,
            skill_section: None,
            clock,
        }
    }

    fn install_project_context(&mut self, context: &protocol::AgentProjectContext) {
        self.cwd.clone_from(&context.cwd);
        self.instructions.clone_from(&context.instructions);
        self.skill_section.clone_from(&context.skill_section);
        self.system_prompt_override
            .clone_from(&context.system_prompt_override);
    }
}

pub struct EngineHandle {
    cmd_tx: mpsc::UnboundedSender<UiCommand>,
    event_tx: mpsc::UnboundedSender<EngineEvent>,
    event_rx: mpsc::UnboundedReceiver<EngineEvent>,
    /// Inbound host-callback requests. Held by the consumer (TUI /
    /// headless app), drained on the main thread, and replied to via
    /// the per-call `oneshot::Sender` embedded in each variant.
    host_rx: mpsc::UnboundedReceiver<HostCall>,
}

impl EngineHandle {
    pub fn send(&self, cmd: UiCommand) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub async fn recv(&mut self) -> Option<EngineEvent> {
        self.event_rx.recv().await
    }

    pub fn try_recv(&mut self) -> Result<EngineEvent, mpsc::error::TryRecvError> {
        self.event_rx.try_recv()
    }

    /// Take ownership of the host-callback receiver. The consumer
    /// holds onto this receiver directly so it can be polled in the
    /// same `tokio::select!` block as `EngineHandle::recv` without
    /// hitting borrow-checker conflicts on `&mut self`.
    pub fn take_host_rx(&mut self) -> mpsc::UnboundedReceiver<HostCall> {
        std::mem::replace(&mut self.host_rx, mpsc::unbounded_channel().1)
    }

    pub fn injector(&self) -> EventInjector {
        EventInjector {
            event_tx: self.event_tx.clone(),
        }
    }

    /// Build a handle backed by caller-owned channels, with no agent task.
    /// Used by the storybook harness to drive UI state without a real engine.
    pub fn for_test() -> (
        Self,
        mpsc::UnboundedReceiver<UiCommand>,
        mpsc::UnboundedSender<EngineEvent>,
    ) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (_host_tx, host_rx) = mpsc::unbounded_channel();
        let handle = EngineHandle {
            cmd_tx,
            event_tx: event_tx.clone(),
            event_rx,
            host_rx,
        };
        (handle, cmd_rx, event_tx)
    }
}

#[derive(Clone)]
pub struct EventInjector {
    event_tx: mpsc::UnboundedSender<EngineEvent>,
}

impl EventInjector {
    pub fn inject_tool_output(&self, call_id: String, chunk: String) {
        let _ = self
            .event_tx
            .send(EngineEvent::ToolOutput { call_id, chunk });
    }
}

/// Start the engine. Must be called from within a tokio runtime.
pub fn start(config: EngineConfig, dispatcher: Box<dyn tools::ToolDispatcher>) -> EngineHandle {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (host_tx, host_rx) = mpsc::unbounded_channel();

    let event_tx_clone = event_tx.clone();
    tokio::spawn(agent::engine_task(
        config, dispatcher, cmd_rx, event_tx, host_tx,
    ));

    EngineHandle {
        cmd_tx,
        event_tx: event_tx_clone,
        event_rx,
        host_rx,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- render_system_prompt ----

    #[test]
    fn render_system_prompt_includes_static_sections_and_extra_instructions() {
        let ctx = PromptContext {
            behavior: SystemPromptBehavior::Interactive,
            capabilities: SystemPromptCapabilities::from_tool_calling(true),
            skills_section: None,
            extra_instructions: Some("MARK-EXTRA-7384"),
        };
        let out = render_system_prompt(&ctx);
        assert!(out.contains("# Managed worktrees"));
        assert!(out.contains("MARK-EXTRA-7384"));
    }

    #[test]
    fn render_system_prompt_collapses_runs_of_blank_lines() {
        let ctx = PromptContext {
            behavior: SystemPromptBehavior::Interactive,
            capabilities: SystemPromptCapabilities::from_tool_calling(true),
            skills_section: None,
            extra_instructions: None,
        };
        let out = render_system_prompt(&ctx);
        // No more than two consecutive newlines anywhere.
        assert!(!out.contains("\n\n\n"));
    }

    #[test]
    fn system_prompt_behavior_switches_interactive_and_autonomous_text() {
        let interactive = build_system_prompt(
            SystemPromptBehavior::Interactive,
            SystemPromptCapabilities::from_tool_calling(true),
            None,
            None,
        );
        let autonomous = build_system_prompt(
            SystemPromptBehavior::Autonomous,
            SystemPromptCapabilities::from_tool_calling(true),
            None,
            None,
        );

        assert!(interactive.contains("You and the user are collaborators"));
        assert!(!interactive.contains("You are running autonomously"));
        assert!(autonomous.contains("You are running autonomously"));
        assert!(!autonomous.contains("You and the user are collaborators"));
    }

    #[test]
    fn system_prompt_omits_tool_guidance_when_tools_disabled() {
        let with_tools = build_system_prompt(
            SystemPromptBehavior::Interactive,
            SystemPromptCapabilities::from_tool_calling(true),
            None,
            None,
        );
        let without_tools = build_system_prompt(
            SystemPromptBehavior::Interactive,
            SystemPromptCapabilities::from_tool_calling(false),
            None,
            None,
        );

        assert!(with_tools.contains("# Tools"));
        assert!(with_tools.contains("read_file"));
        assert!(with_tools.contains("<attached_file ... tool=\"read_file\" already_read=\"true\">"));
        assert!(with_tools.contains("<command_output ... executed_by=\"smelt\">"));
        assert!(with_tools.contains("<skill ... included_by=\"smelt\">"));
        assert!(!without_tools.contains("# Tools"));
        assert!(!without_tools.contains("read_file"));
        assert!(without_tools.contains("# Code"));
    }

    #[test]
    fn system_prompt_is_byte_stable_for_session_inputs() {
        // The same (skills, instructions) must produce identical bytes
        // every time so /mode and cwd switches don't bust the cache.
        let a = build_system_prompt(
            SystemPromptBehavior::Interactive,
            SystemPromptCapabilities::from_tool_calling(true),
            Some("hi"),
            Some("# Skills\nfoo"),
        );
        let b = build_system_prompt(
            SystemPromptBehavior::Interactive,
            SystemPromptCapabilities::from_tool_calling(true),
            Some("hi"),
            Some("# Skills\nfoo"),
        );
        let c = build_system_prompt(
            SystemPromptBehavior::Interactive,
            SystemPromptCapabilities::from_tool_calling(true),
            Some("hi"),
            Some("# Skills\nfoo"),
        );
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn system_prompt_contains_no_time_or_date_substitutions() {
        // Time-varying values (current date, current time, wall-clock
        // timestamps) would silently rotate the cache key. pi-mono embeds
        // `Current date:`; we explicitly don't. Scan the rendered prompt
        // for tokens that would suggest a future regression sneaks one in.
        let prompt = build_system_prompt(
            SystemPromptBehavior::Interactive,
            SystemPromptCapabilities::from_tool_calling(true),
            Some("instructions"),
            Some("# Skills\nfoo"),
        );
        let lower = prompt.to_ascii_lowercase();
        for needle in [
            "current date",
            "current time",
            "today is",
            "current datetime",
            "now is",
        ] {
            assert!(
                !lower.contains(needle),
                "system prompt must not embed a time-varying substring (found `{needle}` in rendered output)",
            );
        }
        // Also pin: rendering twice produces identical bytes - the template
        // itself must not call into any time-of-day source.
        let a = build_system_prompt(
            SystemPromptBehavior::Interactive,
            SystemPromptCapabilities::from_tool_calling(true),
            Some("instructions"),
            Some("# Skills\nfoo"),
        );
        let b = build_system_prompt(
            SystemPromptBehavior::Interactive,
            SystemPromptCapabilities::from_tool_calling(true),
            Some("instructions"),
            Some("# Skills\nfoo"),
        );
        assert_eq!(
            a, b,
            "system prompt drifted between calls with identical inputs",
        );
    }

    // ---- EngineHandle / EventInjector ----

    #[tokio::test]
    async fn for_test_returns_paired_channels_and_send_recv_works() {
        let (mut handle, mut cmd_rx, event_tx) = EngineHandle::for_test();

        handle.send(UiCommand::Cancel);
        let cmd = cmd_rx.recv().await.unwrap();
        assert!(matches!(cmd, UiCommand::Cancel));

        let _ = event_tx.send(EngineEvent::ToolOutput {
            call_id: "id".into(),
            chunk: "x".into(),
        });
        match handle.recv().await.unwrap() {
            EngineEvent::ToolOutput { call_id, chunk } => {
                assert_eq!(call_id, "id");
                assert_eq!(chunk, "x");
            }
            _ => panic!("unexpected event"),
        }
    }

    #[tokio::test]
    async fn event_injector_forwards_tool_output() {
        let (mut handle, _cmd_rx, _event_tx) = EngineHandle::for_test();
        let injector = handle.injector();
        injector.inject_tool_output("c".into(), "out".into());
        match handle.recv().await.unwrap() {
            EngineEvent::ToolOutput { call_id, chunk } => {
                assert_eq!(call_id, "c");
                assert_eq!(chunk, "out");
            }
            _ => panic!("unexpected event"),
        }
    }

    #[tokio::test]
    async fn try_recv_returns_empty_when_no_events_pending() {
        let (mut handle, _cmd_rx, _event_tx) = EngineHandle::for_test();
        let res = handle.try_recv();
        assert!(matches!(res, Err(mpsc::error::TryRecvError::Empty)));
    }
}
