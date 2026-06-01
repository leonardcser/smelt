//! On-disk inputs that feed the agent's system prompt and tool surface.
//!
//! Bundles the five values that share a lifecycle (loaded at startup,
//! refreshed together on `/reload`, shipped together to the engine via
//! [`protocol::UiCommand::ReloadAgentConfig`]) so the rest of the TUI
//! deals with one field instead of five.

use engine::SkillLoader;
use std::path::PathBuf;
use std::sync::Arc;

/// Sources + cached rendered values for everything the agent reads off
/// disk at startup. Live on `TuiApp`; refreshed in place on `/reload`.
#[derive(Default)]
pub struct PromptInputs {
    /// Extra skill search roots from `cfg.skills.paths`. Constant for
    /// the session - there is no Lua API to mutate this yet.
    pub skill_extra_paths: Vec<PathBuf>,
    /// Source file behind `--system-prompt=<path>`. `None` for an
    /// inline string or when the flag was omitted.
    pub system_prompt_path: Option<PathBuf>,

    /// Joined `AGENTS.md` content. Re-read on `/reload` and injected
    /// into the system prompt.
    pub instructions: Option<String>,
    /// Pre-rendered "# Skills" block. Refreshed alongside the
    /// `SkillLoader` so the engine and the local prompt assembly see
    /// the same string.
    pub skill_section: Option<String>,
    /// Cached `--system-prompt` content. Mirror of the engine's
    /// `system_prompt_override`; consulted by compaction, mid-turn mode
    /// change, and `EngineAsk`.
    pub system_prompt_override: Option<String>,
}

/// Bundle returned by [`PromptInputs::refresh`]. The caller swaps the
/// loader into `Core::skills` and emits the [`protocol::UiCommand`]
/// onto the engine task.
pub struct RefreshOutcome {
    pub loader: Arc<SkillLoader>,
    pub system_prompt_read_error: Option<String>,
}

impl PromptInputs {
    /// Load every input from scratch. Called once at startup; the same
    /// work runs again inside [`Self::refresh`] on `/reload`.
    pub fn load(
        skill_extra_paths: Vec<PathBuf>,
        system_prompt_path: Option<PathBuf>,
        instructions: Option<String>,
        system_prompt_override: Option<String>,
    ) -> (Self, Arc<SkillLoader>) {
        let loader = Arc::new(SkillLoader::load(&skill_extra_paths));
        let skill_section = loader.prompt_section().map(String::from);
        let inputs = Self {
            skill_extra_paths,
            system_prompt_path,
            instructions,
            skill_section,
            system_prompt_override,
        };
        (inputs, loader)
    }

    /// Re-read every on-disk source and rebuild the [`SkillLoader`].
    /// Returns the new loader plus any read error so the caller can
    /// surface it via the notifier.
    pub fn refresh(&mut self) -> RefreshOutcome {
        self.instructions = crate::instructions::load();

        let loader = Arc::new(SkillLoader::load(&self.skill_extra_paths));
        self.skill_section = loader.prompt_section().map(String::from);

        let mut err = None;
        if let Some(path) = self.system_prompt_path.clone() {
            match std::fs::read_to_string(&path) {
                Ok(content) => self.system_prompt_override = Some(content),
                Err(e) => err = Some(format!("system-prompt: re-read {}: {e}", path.display())),
            }
        }

        RefreshOutcome {
            loader,
            system_prompt_read_error: err,
        }
    }

    /// Pack the cached values into the wire command the engine consumes
    /// to refresh `EngineConfig` in place.
    pub fn to_reload_command(&self) -> protocol::UiCommand {
        protocol::UiCommand::ReloadAgentConfig {
            instructions: self.instructions.clone(),
            skill_section: self.skill_section.clone(),
            system_prompt_override: self.system_prompt_override.clone(),
        }
    }
}
