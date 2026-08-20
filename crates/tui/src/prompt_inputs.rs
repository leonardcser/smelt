//! On-disk inputs that feed the agent's system prompt and tool surface.
//!
//! Bundles the five values that share a lifecycle (loaded at startup and
//! refreshed together on `/reload`) so the rest of the TUI deals with one
//! field instead of five.

use engine::SkillLoader;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Sources + cached rendered values for everything the agent reads off
/// disk at startup. Live on `TuiApp`; refreshed in place on `/reload`.
pub struct PromptInputs {
    runtime_home: PathBuf,
    config_dir: PathBuf,
    data_dir: PathBuf,
    cwd: PathBuf,
    /// Extra skill search roots from `cfg.skills.paths`. Constant for
    /// the session - there is no Lua API to mutate this yet.
    skill_extra_paths: Vec<PathBuf>,
    /// Source file behind `--system-prompt=<path>`. `None` for an
    /// inline string or when the flag was omitted.
    system_prompt_path: Option<PathBuf>,

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
/// loader into `Core::skills` before publishing the refreshed project context.
pub struct RefreshOutcome {
    pub loader: Arc<SkillLoader>,
    pub system_prompt_read_error: Option<String>,
}

impl PromptInputs {
    pub fn for_runtime(env: &engine::env::RuntimeEnv) -> Self {
        Self {
            runtime_home: env.home().clone(),
            config_dir: env.config_dir().clone(),
            data_dir: env.data_dir().clone(),
            cwd: env.cwd(),
            skill_extra_paths: Vec::new(),
            system_prompt_path: None,
            instructions: None,
            skill_section: None,
            system_prompt_override: None,
        }
    }

    /// Load every input from scratch. Called once at startup; the same
    /// work runs again inside [`Self::refresh`] on `/reload`.
    pub fn load(
        env: &engine::env::RuntimeEnv,
        skill_extra_paths: Vec<PathBuf>,
        system_prompt_path: Option<PathBuf>,
        instructions: Option<String>,
        system_prompt_override: Option<String>,
    ) -> (Self, Arc<SkillLoader>) {
        let mut inputs = Self::for_runtime(env);
        inputs.skill_extra_paths = skill_extra_paths;
        inputs.system_prompt_path = system_prompt_path;
        inputs.instructions = instructions;
        inputs.system_prompt_override = system_prompt_override;
        let loader = inputs.skill_loader_for_cwd(&inputs.cwd);
        inputs.skill_section = loader.prompt_section().map(String::from);
        (inputs, loader)
    }

    pub fn skill_loader_for_cwd(&self, cwd: &Path) -> Arc<SkillLoader> {
        Arc::new(SkillLoader::load_for_runtime(
            &self.skill_extra_paths,
            &self.runtime_home,
            &self.config_dir,
            &self.data_dir,
            cwd,
        ))
    }

    /// Re-read every on-disk source and rebuild the [`SkillLoader`].
    /// Returns the new loader plus any read error so the caller can
    /// surface it via the notifier.
    pub fn refresh(&mut self, cwd: &Path) -> RefreshOutcome {
        self.cwd = cwd.to_path_buf();
        self.instructions = crate::instructions::load(&self.config_dir, &self.cwd);

        let loader = self.skill_loader_for_cwd(&self.cwd);
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
}
