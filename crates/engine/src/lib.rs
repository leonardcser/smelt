mod agent;
pub mod auth;
pub mod cancel;
pub mod catalog;
pub mod clock;
pub(crate) mod config;
pub mod env;
pub mod host;
pub mod image;
pub mod log;

pub mod paths;

pub mod pricing;
pub mod provider;
pub mod redact;
pub(crate) mod result_dedup;
pub(crate) mod skills;
#[cfg(test)]
pub(crate) mod test_util;
pub mod tools;
pub(crate) mod trim;

pub use host::{HostCall, PermissionDecision};
use protocol::{EngineEvent, UiCommand};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Prefix on the user-message slot the compaction plugin uses to carry
/// the handoff summary. The TUI's transcript renderer matches against
/// this prefix to render the message as a `Compacted` block instead of
/// a plain user turn. Source-of-truth for both the bundled Lua plugin
/// and the renderer; they MUST stay byte-equal.
pub const SUMMARY_PREFIX: &str = include_str!("prompts/compact_summary_prefix.md");

/// Marker prefix on synthetic user messages that announce a mode change.
/// The transcript renderer keys on this to display the note as a small
/// inline pill instead of a chat block.
pub const MODE_NOTE_PREFIX: &str = "[smelt:mode] ";

/// Synthetic user-note text appended to the history when the agent's
/// mode switches. Bytes are stable per mode so the prefix that includes
/// these notes still hits the cache on subsequent turns.
pub fn mode_change_note(mode: protocol::AgentMode) -> String {
    let body = match mode {
        protocol::AgentMode::Plan => "now in plan mode. Investigate and reason only; do not modify files or run mutating commands. Use read_file, glob, grep, and read-only bash. edit_file and write_file are unavailable.",
        protocol::AgentMode::Apply => "now in apply mode. You may read, edit, and create files. Continue to confirm destructive bash commands before running them.",
        protocol::AgentMode::Yolo => "now in yolo mode. Full autonomy; act without pausing for confirmation. Continue to avoid genuinely irreversible operations.",
        protocol::AgentMode::Normal => "now in normal mode.",
    };
    format!("{}{}", MODE_NOTE_PREFIX, body)
}

pub use config::ModelConfig;
pub use paths::{config_dir, data_dir, home_dir, state_dir};

/// Re-export so non-engine crates (the TUI) can store an HTTP client
/// without depending on `reqwest` directly.
pub use reqwest::Client as HttpClient;

pub use provider::{Provider, ProviderKind};
pub use skills::SkillLoader;

struct PromptContext<'a> {
    cwd: &'a std::path::Path,
    skills_section: Option<&'a str>,
    extra_instructions: Option<&'a str>,
}

pub(crate) fn build_system_prompt_full(
    cwd: &std::path::Path,
    extra_instructions: Option<&str>,
    skill_section: Option<&str>,
) -> String {
    let ctx = PromptContext {
        cwd,
        skills_section: skill_section,
        extra_instructions,
    };
    render_system_prompt(&ctx)
}

fn render_system_prompt(ctx: &PromptContext<'_>) -> String {
    let template_src = include_str!("prompts/system.txt");
    let env = minijinja::Environment::new();
    let template = env
        .template_from_str(template_src)
        .expect("system prompt template should parse");

    let rendered = template
        .render(minijinja::context! {
            cwd => ctx.cwd.display().to_string(),
            skills_section => ctx.skills_section.unwrap_or(""),
            extra_instructions => ctx.extra_instructions.unwrap_or(""),
        })
        .expect("system prompt template should render");

    let mut result = String::with_capacity(rendered.len());
    let mut blank_count = 0u32;
    for line in rendered.lines() {
        if line.trim().is_empty() {
            blank_count += 1;
            if blank_count <= 2 {
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

#[derive(Clone)]
pub struct ApiConfig {
    pub base: String,
    pub key: String,
    pub key_env: String,
    pub provider_type: String,
    pub model_config: ModelConfig,
}

#[derive(Clone)]
pub struct RequestModelConfig {
    pub model: String,
    pub api: ApiConfig,
}

pub struct EngineConfig {
    pub api: ApiConfig,
    pub model: String,
    pub instructions: Option<String>,
    /// When set, replaces the built-in system prompt template entirely.
    pub system_prompt_override: Option<String>,
    pub cwd: PathBuf,
    /// Pre-rendered "# Skills" block injected into the system prompt.
    /// Built once on startup from the [`SkillLoader`] and refreshed on
    /// `/reload` through [`protocol::UiCommand::ReloadAgentConfig`]. The
    /// loader itself lives on `Core::skills` for tool execution.
    pub skill_section: Option<String>,
    pub redact_secrets: bool,
    /// Use the Anthropic 1-hour cache TTL instead of the default 5-minute
    /// one when emitting `cache_control` markers.
    pub cache_ttl_long: bool,
    /// Source of monotonic + wall-clock time. Production uses
    /// [`clock::RealClock`]; deterministic-simulation harnesses inject a
    /// [`clock::VirtualClock`] so scenarios can replay against advanced time.
    pub clock: Arc<dyn clock::Clock>,
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
    fn render_system_prompt_includes_cwd_and_extra_instructions() {
        let cwd = std::path::Path::new("/tmp/x");
        let ctx = PromptContext {
            cwd,
            skills_section: None,
            extra_instructions: Some("MARK-EXTRA-7384"),
        };
        let out = render_system_prompt(&ctx);
        assert!(out.contains("/tmp/x"));
        assert!(out.contains("MARK-EXTRA-7384"));
    }

    #[test]
    fn render_system_prompt_collapses_runs_of_blank_lines() {
        let cwd = std::path::Path::new("/x");
        let ctx = PromptContext {
            cwd,
            skills_section: None,
            extra_instructions: None,
        };
        let out = render_system_prompt(&ctx);
        // No more than two consecutive newlines anywhere.
        assert!(!out.contains("\n\n\n"));
    }

    #[test]
    fn system_prompt_is_byte_stable_for_session_inputs() {
        // The same (cwd, skills, instructions) must produce identical bytes
        // every time so /mode switches don't bust the cache.
        let cwd = std::path::Path::new("/x");
        let a = build_system_prompt_full(cwd, Some("hi"), Some("# Skills\nfoo"));
        let b = build_system_prompt_full(cwd, Some("hi"), Some("# Skills\nfoo"));
        assert_eq!(a, b);
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
