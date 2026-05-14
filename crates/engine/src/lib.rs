mod agent;
pub mod auth;
pub(crate) mod cancel;
pub(crate) mod compact;
pub(crate) mod config;
pub mod image;
pub mod log;

pub mod paths;

pub mod pricing;
pub mod provider;
pub mod redact;
pub(crate) mod result_dedup;
pub(crate) mod skills;
pub mod tools;
pub(crate) mod trim;

use protocol::{EngineEvent, UiCommand};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Default auto-compaction threshold, as a percentage of the context window.
const DEFAULT_COMPACT_THRESHOLD_PERCENT: u64 = 80;

/// Environment variable that overrides the auto-compaction threshold.
/// Accepts an integer percentage in `[10, 95]`.
const COMPACT_THRESHOLD_ENV: &str = "SMELT_COMPACT_THRESHOLD_PERCENT";

/// Auto-compaction threshold as a percentage of the context window.
/// Reads `SMELT_COMPACT_THRESHOLD_PERCENT` at call time; falls back to 80.
pub fn compact_threshold_percent() -> u64 {
    std::env::var(COMPACT_THRESHOLD_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|p| (10..=95).contains(p))
        .unwrap_or(DEFAULT_COMPACT_THRESHOLD_PERCENT)
}

pub use compact::SUMMARY_PREFIX;
pub use config::ModelConfig;
pub use paths::{config_dir, data_dir, home_dir, state_dir};

pub use provider::{Provider, ProviderKind};
pub use skills::SkillLoader;

struct PromptContext<'a> {
    cwd: &'a std::path::Path,
    write_access: bool,
    skills_section: Option<&'a str>,
    extra_instructions: Option<&'a str>,
}

pub(crate) fn build_system_prompt_full(
    mode: protocol::AgentMode,
    cwd: &std::path::Path,
    extra_instructions: Option<&str>,
    skill_section: Option<&str>,
) -> String {
    let ctx = PromptContext {
        cwd,
        write_access: matches!(mode, protocol::AgentMode::Apply | protocol::AgentMode::Yolo),
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
            write_access => ctx.write_access,
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

#[derive(Clone, Default)]
pub struct AuxiliaryModelConfig {
    pub title: Option<RequestModelConfig>,
    pub prediction: Option<RequestModelConfig>,
    pub compaction: Option<RequestModelConfig>,
    pub btw: Option<RequestModelConfig>,
}

pub struct EngineConfig {
    pub api: ApiConfig,
    pub model: String,
    /// Per-task model overrides; `None` falls back to the live primary.
    pub auxiliary: AuxiliaryModelConfig,
    pub instructions: Option<String>,
    /// When set, replaces the built-in system prompt template entirely.
    pub system_prompt_override: Option<String>,
    pub cwd: PathBuf,
    pub skills: Option<Arc<SkillLoader>>,
    pub auto_compact: bool,
    /// `None` causes the engine to fetch this from the provider API on first use.
    pub context_window: Option<u32>,
    pub redact_secrets: bool,
}

pub use protocol::AuxiliaryTask;

impl EngineConfig {
    /// Returns the model+api for an auxiliary task, falling back to the primary.
    pub(crate) fn aux_or_primary(&self, task: AuxiliaryTask) -> RequestModelConfig {
        let slot = match task {
            AuxiliaryTask::Title => &self.auxiliary.title,
            AuxiliaryTask::Prediction => &self.auxiliary.prediction,
            AuxiliaryTask::Compaction => &self.auxiliary.compaction,
            AuxiliaryTask::Btw => &self.auxiliary.btw,
        };
        slot.clone().unwrap_or_else(|| RequestModelConfig {
            model: self.model.clone(),
            api: self.api.clone(),
        })
    }
}

pub struct EngineHandle {
    cmd_tx: mpsc::UnboundedSender<UiCommand>,
    event_tx: mpsc::UnboundedSender<EngineEvent>,
    event_rx: mpsc::UnboundedReceiver<EngineEvent>,
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
        let handle = EngineHandle {
            cmd_tx,
            event_tx: event_tx.clone(),
            event_rx,
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

    let event_tx_clone = event_tx.clone();
    tokio::spawn(agent::engine_task(config, dispatcher, cmd_rx, event_tx));

    EngineHandle {
        cmd_tx,
        event_tx: event_tx_clone,
        event_rx,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- compact_threshold_percent ----

    fn with_env<F: FnOnce()>(key: &str, value: Option<&str>, f: F) {
        let prev = std::env::var(key).ok();
        unsafe {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        f();
        unsafe {
            match prev {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }

    #[test]
    fn compact_threshold_defaults_to_80_when_env_unset() {
        with_env(COMPACT_THRESHOLD_ENV, None, || {
            assert_eq!(compact_threshold_percent(), 80);
        });
    }

    #[test]
    fn compact_threshold_reads_valid_env_value() {
        with_env(COMPACT_THRESHOLD_ENV, Some("50"), || {
            assert_eq!(compact_threshold_percent(), 50);
        });
    }

    #[test]
    fn compact_threshold_rejects_below_10_and_above_95() {
        with_env(COMPACT_THRESHOLD_ENV, Some("9"), || {
            assert_eq!(compact_threshold_percent(), 80);
        });
        with_env(COMPACT_THRESHOLD_ENV, Some("96"), || {
            assert_eq!(compact_threshold_percent(), 80);
        });
    }

    #[test]
    fn compact_threshold_accepts_inclusive_bounds() {
        with_env(COMPACT_THRESHOLD_ENV, Some("10"), || {
            assert_eq!(compact_threshold_percent(), 10);
        });
        with_env(COMPACT_THRESHOLD_ENV, Some("95"), || {
            assert_eq!(compact_threshold_percent(), 95);
        });
    }

    #[test]
    fn compact_threshold_rejects_non_numeric_values() {
        with_env(COMPACT_THRESHOLD_ENV, Some("nope"), || {
            assert_eq!(compact_threshold_percent(), 80);
        });
    }

    #[test]
    fn compact_threshold_trims_whitespace_around_value() {
        with_env(COMPACT_THRESHOLD_ENV, Some("  42 "), || {
            assert_eq!(compact_threshold_percent(), 42);
        });
    }

    // ---- render_system_prompt ----

    #[test]
    fn render_system_prompt_includes_cwd_and_extra_instructions() {
        let cwd = std::path::Path::new("/tmp/x");
        let ctx = PromptContext {
            cwd,
            write_access: true,
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
            write_access: false,
            skills_section: None,
            extra_instructions: None,
        };
        let out = render_system_prompt(&ctx);
        // No more than two consecutive newlines anywhere.
        assert!(!out.contains("\n\n\n"));
    }

    // ---- build_system_prompt_full mode flag ----

    #[test]
    fn build_system_prompt_full_chooses_write_access_by_agent_mode() {
        // Just verify the helper runs and produces non-empty output for both modes.
        let cwd = std::path::Path::new("/x");
        let plan = build_system_prompt_full(protocol::AgentMode::Plan, cwd, None, None);
        let apply = build_system_prompt_full(protocol::AgentMode::Apply, cwd, None, None);
        assert!(!plan.is_empty());
        assert!(!apply.is_empty());
    }

    // ---- aux_or_primary ----

    fn primary_cfg() -> RequestModelConfig {
        RequestModelConfig {
            model: "primary".into(),
            api: ApiConfig {
                base: "base".into(),
                key: "k".into(),
                key_env: "K".into(),
                provider_type: "openai".into(),
                model_config: ModelConfig::default(),
            },
        }
    }

    fn aux_cfg(name: &str) -> RequestModelConfig {
        let mut p = primary_cfg();
        p.model = name.into();
        p
    }

    fn engine_with(aux: AuxiliaryModelConfig) -> EngineConfig {
        let primary = primary_cfg();
        EngineConfig {
            api: primary.api,
            model: primary.model,
            auxiliary: aux,
            instructions: None,
            system_prompt_override: None,
            cwd: PathBuf::from("/"),
            skills: None,
            auto_compact: false,
            context_window: None,
            redact_secrets: false,
        }
    }

    #[test]
    fn aux_or_primary_returns_override_when_set() {
        let eng = engine_with(AuxiliaryModelConfig {
            title: Some(aux_cfg("title-model")),
            ..Default::default()
        });
        assert_eq!(
            eng.aux_or_primary(AuxiliaryTask::Title).model,
            "title-model"
        );
    }

    #[test]
    fn aux_or_primary_falls_back_to_primary_when_slot_empty() {
        let eng = engine_with(AuxiliaryModelConfig::default());
        for task in [
            AuxiliaryTask::Title,
            AuxiliaryTask::Prediction,
            AuxiliaryTask::Compaction,
            AuxiliaryTask::Btw,
        ] {
            assert_eq!(eng.aux_or_primary(task).model, "primary");
        }
    }

    #[test]
    fn aux_or_primary_threads_each_slot_independently() {
        let eng = engine_with(AuxiliaryModelConfig {
            title: Some(aux_cfg("t")),
            prediction: Some(aux_cfg("p")),
            compaction: Some(aux_cfg("c")),
            btw: Some(aux_cfg("b")),
        });
        assert_eq!(eng.aux_or_primary(AuxiliaryTask::Title).model, "t");
        assert_eq!(eng.aux_or_primary(AuxiliaryTask::Prediction).model, "p");
        assert_eq!(eng.aux_or_primary(AuxiliaryTask::Compaction).model, "c");
        assert_eq!(eng.aux_or_primary(AuxiliaryTask::Btw).model, "b");
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
