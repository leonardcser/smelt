//! Operations called by Lua bindings through `with_app`.

use crate::app::{LuaBringUpError, TuiApp};
use smelt_core::transcript_model::ConfirmChoice;

enum LuaReloadKind {
    Manual,
    AutoConfig,
}

fn bring_up_error(
    phase: &'static str,
    path: Option<std::path::PathBuf>,
    message: String,
) -> LuaBringUpError {
    LuaBringUpError {
        message,
        location: smelt_core::lua::LuaLoadFailureLocation { phase, path },
    }
}

struct LuaTuiGeneration {
    ui: crate::smelt_edit::Ui,
    paint_registry: crate::lua::paint::PaintRegistry,
    picker_state: std::collections::HashMap<crate::smelt_edit::WinId, crate::picker::PickerState>,
    placeholders: std::collections::HashMap<crate::smelt_edit::WinId, String>,
    placeholder_opts:
        std::collections::HashMap<crate::smelt_edit::WinId, crate::app::PlaceholderOpts>,
    busy_stack: crate::app::BusyStack,
    prompt_placeholder_display: Option<String>,
}

impl LuaReloadKind {
    fn refresh_agent_inputs(&self) -> bool {
        matches!(self, Self::Manual)
    }
}

impl TuiApp {
    /// Run a Lua command line through the shared dispatcher. Bare names (`"btw foo"`)
    /// are normalized to prompt-command syntax so Lua APIs keep their historical shape,
    /// while explicit `:`, `/`, and `!` prefixes keep their typed meaning.
    pub(crate) fn apply_lua_command(&mut self, line: &str) {
        let trimmed = line.trim_start();
        let normalized: std::borrow::Cow<str> =
            if trimmed.starts_with('/') || trimmed.starts_with(':') || trimmed.starts_with('!') {
                std::borrow::Cow::Borrowed(line)
            } else {
                std::borrow::Cow::Owned(format!("/{trimmed}"))
            };
        match crate::commands::run_command_with_context(
            self,
            &normalized,
            crate::commands::CommandContext::lua(),
        ) {
            crate::app::CommandAction::Exec(handle) => {
                self.exec = Some(handle);
            }
            crate::app::CommandAction::Continue => {}
        }
    }

    /// `/reload` entry point. Wraps [`Self::bring_up_lua`] (the
    /// shared cold-start + reload pipeline) with a user-facing toast.
    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn reload_lua(&mut self) {
        self.reload_lua_inner(LuaReloadKind::Manual);
    }

    /// Auto-reload entry point for Lua config edits. Keeps prompt inputs stable
    /// so changing AGENTS.md, skills, or `--system-prompt` only affects the
    /// agent after a manual `/reload`.
    pub(crate) fn reload_lua_config(&mut self) {
        self.reload_lua_inner(LuaReloadKind::AutoConfig);
    }

    fn reload_lua_inner(&mut self, kind: LuaReloadKind) {
        self.pending_lua_reload = false;
        self.pending_lua_reload_refresh_agent_inputs = false;
        let err = self.bring_up_lua("reload", kind.refresh_agent_inputs());
        match err {
            Some(error) => {
                let message = format!("lua reload: {error}");
                if self
                    .lua_reload_failure
                    .as_ref()
                    .is_none_or(|failure| failure.message != message)
                {
                    self.notify_error_sticky(message.clone());
                }
                self.lua_reload_failure = Some(LuaBringUpError {
                    message,
                    location: error.location,
                });
            }
            None if self.lua.warnings().is_empty() => {
                self.lua_reload_failure = None;
                self.notify("lua reloaded".into());
            }
            None => {
                self.lua_reload_failure = None;
            }
        }
    }

    /// Mark a full reload for the next point where no turn or modal can hold
    /// callbacks that the reload would wipe. Returns `true` for a new request
    /// and `false` when one was already pending.
    pub(crate) fn schedule_lua_reload(&mut self) -> bool {
        let was_pending = self.pending_lua_reload;
        self.pending_lua_reload = true;
        self.pending_lua_reload_refresh_agent_inputs = true;
        !was_pending
    }

    /// Mark a Lua-config-only reload for auto-reload. If a full reload is
    /// already pending, keep it full.
    pub(crate) fn schedule_lua_config_reload(&mut self) -> bool {
        if self.pending_lua_reload {
            return false;
        }
        self.pending_lua_reload = true;
        self.pending_lua_reload_refresh_agent_inputs = false;
        true
    }

    pub(crate) fn schedule_runtime_reconcile(&mut self) {
        self.pending_runtime_reconcile = true;
    }

    pub(crate) fn drain_idle_work(&mut self) -> bool {
        let mut did_work = self.dismiss_expired_notification();
        did_work |= self.expire_pending_keymap_chord();
        did_work |= self.flush_due_tool_drafts();
        did_work |= self.poll_managed_auth();
        did_work |= self.try_perform_scheduled_runtime_reconcile();
        did_work |= self.try_perform_scheduled_cwd_change();
        did_work |= self.try_perform_scheduled_lua_reload();
        did_work
    }

    pub(crate) fn try_perform_scheduled_runtime_reconcile(&mut self) -> bool {
        if !self.pending_runtime_reconcile {
            return false;
        }
        self.pending_runtime_reconcile = false;
        if let Err(error) = self.reconcile_committed_lua_runtime() {
            self.notify_error_sticky(error);
        }
        true
    }

    fn try_perform_scheduled_lua_reload(&mut self) -> bool {
        if !self.pending_lua_reload || !self.can_reload_lua_now() {
            return false;
        }
        let kind = if self.pending_lua_reload_refresh_agent_inputs {
            LuaReloadKind::Manual
        } else {
            LuaReloadKind::AutoConfig
        };
        self.pending_lua_reload = false;
        self.pending_lua_reload_refresh_agent_inputs = false;
        self.reload_lua_inner(kind);
        true
    }

    pub(crate) fn can_reload_lua_now(&self) -> bool {
        !self.prompt_input_is_busy() && self.ui.active_modal().is_none()
    }

    /// Build and commit a fresh Lua generation. This pipeline is shared by
    /// interactive launch, manual reload, and automatic config reload.
    ///
    /// Candidate evaluation uses fresh Lua registries plus an isolated fork of
    /// generation-owned TUI state. The committed runtime, resolved values,
    /// callbacks, UI resources, managers, and ready hooks remain untouched when
    /// loading or pure runtime resolution fails. On success the old generation
    /// is retired, staged TUI state and declarations become live, synchronous
    /// effects run in explicit order, and only then are `ready` hooks drained.
    /// Manual reloads additionally refresh AGENTS.md, skills, and explicit
    /// system-prompt inputs; automatic reloads leave those inputs unchanged.
    ///
    /// Must be called inside an `install_app_ptr` scope. Returns a candidate
    /// load or resolution error without changing the committed generation.
    pub(crate) fn bring_up_lua(
        &mut self,
        kind: &'static str,
        refresh_agent_inputs: bool,
    ) -> Option<LuaBringUpError> {
        self.bring_up_lua_at(kind, refresh_agent_inputs, None)
    }

    pub(crate) fn bring_up_lua_for_cwd(
        &mut self,
        path: std::path::PathBuf,
        mark_session_dirty: bool,
    ) -> Option<LuaBringUpError> {
        self.bring_up_lua_at("cwd", true, Some((path, mark_session_dirty)))
    }

    fn bring_up_lua_at(
        &mut self,
        kind: &'static str,
        refresh_agent_inputs: bool,
        cwd_transition: Option<(std::path::PathBuf, bool)>,
    ) -> Option<LuaBringUpError> {
        if matches!(kind, "reload" | "cwd") {
            if let Some(error) = self.lua.flush_persistent_state() {
                return Some(bring_up_error(
                    "state_flush",
                    None,
                    format!("flush persistent state: {error}"),
                ));
            }
        }

        let target_cwd = cwd_transition
            .as_ref()
            .map(|(path, _)| path.clone())
            .unwrap_or_else(|| self.core.env.cwd().clone());
        let candidate_skills = std::sync::Arc::new(engine::SkillLoader::load_for_cwd(
            &self.prompt_inputs.skill_extra_paths,
            &target_cwd,
        ));
        let retired_generation = self.lua.id;
        let candidate_id = retired_generation.wrapping_add(1);
        let committed_tui = self.begin_lua_tui_candidate();
        let candidate_result = self.lua.load_candidate(
            candidate_id,
            Some(&target_cwd),
            candidate_skills,
            self.lua_wakeup_tx.clone(),
        );
        let mut candidate_tui = self.finish_lua_tui_candidate(committed_tui);
        let candidate = match candidate_result {
            Ok(candidate) => candidate,
            Err(failed) => {
                self.discard_lua_candidate_resources(candidate_id);
                return Some(LuaBringUpError {
                    message: failed.message,
                    location: failed.location,
                });
            }
        };
        let (next_runtime, next_managed_models) =
            match self.resolve_lua_runtime_config(candidate.desired()) {
                Ok(runtime) => runtime,
                Err(error) => {
                    self.discard_lua_candidate_resources(candidate_id);
                    return Some(bring_up_error("runtime_resolution", None, error));
                }
            };
        let next_permissions = smelt_core::permissions::resolve_permissions(
            &candidate.desired().permissions.rules,
            &candidate.desired().permissions.tool_defaults,
            candidate.desired().modes.behaviors.clone(),
            &next_runtime.settings,
            &target_cwd,
            self.core.permissions.paths_fn(),
        );
        for callback in candidate_tui
            .ui
            .finish_lua_generation(smelt_core::lua::LUA_BUF_ID_BASE)
        {
            candidate.remove_callback(callback);
        }
        candidate_tui.paint_registry.finish_lua_generation();
        candidate_tui
            .picker_state
            .retain(|win, _| candidate_tui.ui.win(*win).is_some());
        candidate_tui
            .placeholders
            .retain(|win, _| candidate_tui.ui.win(*win).is_some());
        candidate_tui
            .placeholder_opts
            .retain(|win, _| candidate_tui.ui.win(*win).is_some());
        let staged_cwd = if let Some((path, _)) = &cwd_transition {
            match Self::stage_process_cwd(path.clone()) {
                Ok(staged) => Some(staged),
                Err(error) => {
                    self.discard_lua_candidate_resources(candidate_id);
                    return Some(bring_up_error("cwd", Some(path.clone()), error));
                }
            }
        } else {
            None
        };
        if let Err(error) = candidate.activate() {
            self.discard_lua_candidate_resources(candidate_id);
            return Some(bring_up_error(
                "activation",
                None,
                format!("activate Lua candidate: {error}"),
            ));
        }
        let cwd_commit = match (&staged_cwd, &cwd_transition) {
            (Some(staged), Some((_, mark_session_dirty))) => {
                Some((staged.cwd().to_path_buf(), *mark_session_dirty))
            }
            (None, None) => None,
            _ => unreachable!("staged cwd must match the requested transition"),
        };

        self.core.signals.clear_lua_generation(retired_generation);
        self.core.timers.clear_generation(retired_generation);
        self.lua.retire();
        self.commit_lua_tui_candidate(candidate_tui);
        self.lua = candidate;
        self.command_catalog
            .activate(self.lua.command_names_handle());
        if let Some(prompt) = self.ui.buf_mut(crate::app::PROMPT_EDIT_BUF) {
            prompt.invalidate_render_cache();
        }
        self.core.lua_generation = self.lua.id;
        let lua_shared = std::sync::Arc::clone(self.lua.shared());
        if let Err(error) = crate::lua::api::terminal::commit_staged_title(&lua_shared) {
            self.notify_error_sticky(format!("terminal title: {error}"));
        }
        crate::lua::api::notify::commit_staged_notices(&lua_shared);
        lua_shared.commit_staged_logs();
        for warning in self.lua.warnings().to_vec() {
            self.notify_warn(warning);
        }
        self.managed_models = next_managed_models;
        self.commit_lua_runtime_config(next_runtime, next_permissions);
        if let Some((cwd, mark_session_dirty)) = &cwd_commit {
            self.install_runtime_cwd(cwd.clone(), *mark_session_dirty);
        }
        let committed_cwd = match (staged_cwd, cwd_commit) {
            (Some(staged), Some((_, mark_session_dirty))) => {
                staged.commit();
                Some(mark_session_dirty)
            }
            (None, None) => None,
            _ => unreachable!("staged cwd must match the requested transition"),
        };
        self.submit_managed_model_refreshes();
        self.reconcile_auto_reload();
        if refresh_agent_inputs {
            self.refresh_agent_inputs();
        }
        if let Some(mark_session_dirty) = committed_cwd {
            self.sync_inline_options();
            self.publish_cwd_change(mark_session_dirty);
            self.pending_lua_reload = false;
            self.pending_lua_reload_refresh_agent_inputs = false;
        } else if refresh_agent_inputs {
            self.publish_agent_project_context();
        }
        self.publish_diff_signals();
        self.reconcile_runtime_controllers();

        // Make layout geometry current before `ready` hooks open overlays or
        // query `Win:rect()`. Without this, cold-start hooks see the seed layout
        // until the first render, while resize/reload paths see the Lua layout.
        self.refresh_main_layout();
        let hook_errors = self.lua.drain_lifecycle_hooks("ready", move |lua| {
            let t = lua.create_table()?;
            t.set("kind", kind)?;
            Ok::<mlua::Value, mlua::Error>(mlua::Value::Table(t))
        });
        for error in hook_errors {
            self.notify_error_sticky(error);
        }
        None
    }

    fn discard_lua_candidate_resources(&mut self, generation: u64) {
        self.core.signals.clear_lua_generation(generation);
        self.core.timers.clear_generation(generation);
    }

    /// Re-read filesystem-backed inputs that feed the agent's system prompt.
    /// The caller publishes them with the rest of the project context after
    /// every part of the transaction has committed.
    pub(crate) fn refresh_agent_inputs(&mut self) {
        let outcome = self.prompt_inputs.refresh();
        self.core.skills = Some(outcome.loader);
        if let Some(err) = outcome.system_prompt_read_error {
            self.notify_error_sticky(err);
        }
    }

    pub(crate) fn reconcile_committed_lua_runtime(&mut self) -> Result<(), String> {
        self.lua.refresh_desired_state()?;
        self.reconcile_runtime_snapshot()
    }

    pub(crate) fn reconcile_runtime_snapshot(&mut self) -> Result<(), String> {
        let desired = self.lua.desired().clone();
        let (next, managed_models) = self.resolve_lua_runtime_config(&desired)?;
        let permissions = smelt_core::permissions::resolve_permissions(
            &desired.permissions.rules,
            &desired.permissions.tool_defaults,
            desired.modes.behaviors,
            &next.settings,
            &self.core.env.cwd(),
            self.core.permissions.paths_fn(),
        );
        self.managed_models = managed_models;
        self.commit_lua_runtime_config(next, permissions);
        self.submit_managed_model_refreshes();
        self.reconcile_runtime_controllers();
        self.publish_diff_signals();
        self.drain_signals_pending();
        Ok(())
    }

    fn resolve_lua_runtime_config(
        &self,
        desired: &crate::lua::LuaDesiredState,
    ) -> Result<(smelt_core::RuntimeState, smelt_core::ManagedModels), String> {
        let mut config = desired.config.clone();
        let mut managed_models = self.managed_models.clone();
        managed_models.inject_oauth_providers(&mut config);
        managed_models.sync_desired(&config, self.core.config.revision.wrapping_add(1));
        let mut available_models = config.resolve_models();
        managed_models.inject(&config, &mut available_models);
        let mut runtime = smelt_core::resolve_runtime(smelt_core::RuntimeInputs {
            config: &config,
            startup: &self.core.startup_overrides,
            available_models: &available_models,
            registered_modes: &desired.modes.cycle,
            selections: &smelt_core::RuntimeSelections::default(),
            previous: Some(&self.core.config),
            headless: false,
        })
        .map_err(|error| format!("runtime config reconciliation failed: {error}"))?;
        if let Some(active) = runtime.active_model_mut() {
            let provider = engine::auth::AuthProvider::from_provider_type(&active.provider_type);
            if provider.is_some_and(|provider| !managed_models.provider(provider).authenticated) {
                active.availability = smelt_core::ModelAvailability::Unavailable {
                    reason: smelt_core::ModelUnavailableReason::MissingCredentials,
                };
            }
        }
        Ok((runtime, managed_models))
    }

    pub(crate) fn reconcile_permissions(&mut self) {
        let desired = self.lua.desired();
        let permission_resolution = smelt_core::permissions::resolve_permissions(
            &desired.permissions.rules,
            &desired.permissions.tool_defaults,
            desired.modes.behaviors.clone(),
            &self.core.config.settings,
            &self.core.env.cwd(),
            self.core.permissions.paths_fn(),
        );
        self.core
            .permissions
            .apply_resolution(permission_resolution);
    }

    fn commit_lua_runtime_config(
        &mut self,
        mut next: smelt_core::RuntimeState,
        permissions: smelt_core::permissions::PermissionResolution,
    ) {
        let old_settings = self.core.config.settings.clone();
        let old_mode = self.core.config.mode.clone();
        let old_reasoning = self.core.config.reasoning_effort;
        let old_model = self.core.config.active_model().cloned();
        let changed = next != self.core.config;
        if changed {
            next.revision = self.core.config.revision.wrapping_add(1);
            self.core.config = next;
            self.apply_settings_effects(&old_settings);
            if self.core.config.mode != old_mode {
                self.core.signals.set_dyn(
                    "agent_mode",
                    std::rc::Rc::new(self.core.config.mode.as_str().to_string()),
                );
            }
            if self.core.config.reasoning_effort != old_reasoning {
                self.core.signals.set_dyn(
                    "reasoning",
                    std::rc::Rc::new(self.core.config.reasoning_effort.label().to_string()),
                );
            }
            if self.core.config.active_model() != old_model.as_ref() {
                self.core.signals.set_dyn(
                    "model",
                    std::rc::Rc::new(
                        self.core
                            .config
                            .active_model()
                            .map(|model| model.key.clone()),
                    ),
                );
                self.refresh_context_window();
                self.warn_if_api_base_normalized();
            }
        }

        self.core.permissions.apply_resolution(permissions);
    }

    /// Reconcile MCP servers against the post-reload `smelt.mcp.register`
    /// desired-state set. Spawns/stops/restarts servers off-thread; the
    /// TUI continues without blocking on handshakes. The
    /// [`smelt_core::mcp::McpDispatcher`] reads tool defs live from the
    /// manager, so the engine's dispatch path picks up the new server
    /// set without further coordination.
    fn reconcile_runtime_controllers(&mut self) {
        self.reconcile_mcp_servers();
        self.lua
            .shared()
            .lsp
            .configure_detached(self.core.config.lsp.clone());
    }

    pub(crate) fn reconcile_mcp_servers(&mut self) {
        let Some(manager) = self.core.mcp.clone() else {
            return;
        };
        manager.reconcile_detached(self.core.config.mcp.clone());
    }

    /// Replace live TUI generation projections with isolated candidate copies.
    /// Candidate module bodies can freely refresh named buffers, windows,
    /// overlays, paint slots, callbacks, and theme state without making those
    /// changes observable before the Lua transaction commits.
    fn begin_lua_tui_candidate(&mut self) -> LuaTuiGeneration {
        let mut candidate_ui = self.ui.fork_for_lua_generation();
        let _ = candidate_ui.reap_anonymous(smelt_core::lua::LUA_BUF_ID_BASE);
        let candidate_paint_registry = self.paint_registry.fork_for_lua_generation();
        let mut candidate_placeholders = self.placeholders.clone();
        candidate_placeholders.retain(|win, _| candidate_ui.win(*win).is_some());
        let mut candidate_placeholder_opts = self.placeholder_opts.clone();
        candidate_placeholder_opts.retain(|win, _| candidate_ui.win(*win).is_some());
        let prompt_placeholder_display = self
            .prompt_placeholder_display
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();

        LuaTuiGeneration {
            ui: std::mem::replace(&mut self.ui, candidate_ui),
            paint_registry: std::mem::replace(&mut self.paint_registry, candidate_paint_registry),
            picker_state: std::mem::take(&mut self.picker_state),
            placeholders: std::mem::replace(&mut self.placeholders, candidate_placeholders),
            placeholder_opts: std::mem::replace(
                &mut self.placeholder_opts,
                candidate_placeholder_opts,
            ),
            busy_stack: std::mem::take(&mut self.busy_stack),
            prompt_placeholder_display,
        }
    }

    /// Restore the committed TUI state after candidate evaluation and return
    /// the isolated candidate state for either commit or discard.
    fn finish_lua_tui_candidate(&mut self, committed: LuaTuiGeneration) -> LuaTuiGeneration {
        let candidate_prompt_placeholder_display = self
            .prompt_placeholder_display
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        *self
            .prompt_placeholder_display
            .lock()
            .unwrap_or_else(|error| error.into_inner()) =
            committed.prompt_placeholder_display.clone();

        LuaTuiGeneration {
            ui: std::mem::replace(&mut self.ui, committed.ui),
            paint_registry: std::mem::replace(&mut self.paint_registry, committed.paint_registry),
            picker_state: std::mem::replace(&mut self.picker_state, committed.picker_state),
            placeholders: std::mem::replace(&mut self.placeholders, committed.placeholders),
            placeholder_opts: std::mem::replace(
                &mut self.placeholder_opts,
                committed.placeholder_opts,
            ),
            busy_stack: std::mem::replace(&mut self.busy_stack, committed.busy_stack),
            prompt_placeholder_display: candidate_prompt_placeholder_display,
        }
    }

    fn commit_lua_tui_candidate(&mut self, mut candidate: LuaTuiGeneration) {
        candidate.ui.merge_rust_callbacks_from(&mut self.ui);
        self.ui = candidate.ui;
        self.paint_registry = candidate.paint_registry;
        self.picker_state = candidate.picker_state;
        self.placeholders = candidate.placeholders;
        self.placeholder_opts = candidate.placeholder_opts;
        self.busy_stack = candidate.busy_stack;
        *self
            .prompt_placeholder_display
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = candidate.prompt_placeholder_display;
    }

    pub(crate) fn rewind_active_user_turn_if_no_output(
        &mut self,
        restore_vim_insert: bool,
    ) -> bool {
        let Some(turn) = self.agent.as_ref() else {
            return false;
        };
        let Some(block_idx) = turn.rewind_block_idx else {
            return false;
        };
        if !self.queued_inputs.is_empty() || turn.assistant_output_started {
            return false;
        }
        self.rewind_to_block(Some(block_idx), restore_vim_insert);
        true
    }

    /// Rewind to a transcript block, or to before the first turn when `block_idx` is `None`.
    pub(crate) fn rewind_to_block(&mut self, block_idx: Option<usize>, restore_vim_insert: bool) {
        if let Some(bidx) = block_idx {
            if self.agent.is_some() {
                self.cancel_agent();
                self.agent = None;
            }
            if let Some((text, images)) = self.rewind_to(bidx) {
                self.clear_prompt_prediction();
                let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
                self.input.restore_from_rewind(&mut pctx, text, images);
            }
            while self.core.engine.try_recv().is_ok() {}
            self.save_session();
        } else {
            if self.agent.is_some() {
                self.cancel_agent();
                self.agent = None;
            }
            self.clear_prompt_prediction();
            self.rewind_to_start();
            while self.core.engine.try_recv().is_ok() {}
            self.save_session();
            if restore_vim_insert {
                let win = self
                    .ui
                    .win_mut(crate::app::PROMPT_WIN)
                    .expect("prompt window");
                self.input
                    .set_vim_mode(win, crate::smelt_edit::VimMode::Insert);
            }
        }
    }

    /// Load a saved session by id, refresh screen, and scroll to bottom.
    pub(crate) fn load_session_by_id(&mut self, id: &str) {
        let target_rows = crate::app::transcript::descriptor_tail_target_rows(self.last_height);
        let resume =
            match smelt_core::session::load_store_resume_result(id, self.last_width, target_rows) {
                Ok(Some(resume)) => resume,
                Ok(None) => {
                    self.notify_error_sticky(format!("session {id:?} has no stored state"));
                    return;
                }
                Err(err) => {
                    self.notify_error_sticky(format!("failed to load session: {err}"));
                    return;
                }
            };
        let smelt_core::session::SessionStoreResume {
            header,
            store_ref,
            head,
            descriptor_tail,
        } = resume;
        let degraded_warnings = header.degraded_warnings.clone();
        let (transcript, repair_descriptors) =
            match crate::app::transcript::LoadedTranscript::from_descriptor_slice(
                descriptor_tail,
                store_ref.session_dir.clone(),
            ) {
                Some(transcript) => (transcript, false),
                None => match crate::app::history::materialize_full_transcript_read_only_result(
                    &self.lua, id,
                ) {
                    Ok(Some(transcript)) => (transcript, true),
                    Ok(None) => {
                        self.notify_error_sticky(format!(
                            "session {id:?} has no readable transcript state"
                        ));
                        return;
                    }
                    Err(err) => {
                        self.notify_error_sticky(format!(
                            "failed to load session transcript: {err}"
                        ));
                        return;
                    }
                },
            };
        let document = crate::app::session_document::SessionDocument::from_store(
            header,
            store_ref,
            head,
            transcript,
            self.core.env.pid(),
            self.core.env.cwd(),
        );
        let mut document = document.into_store_backed();
        if repair_descriptors {
            document = document.requiring_descriptor_repair();
        }
        self.load_store_backed_session(document);
        if !degraded_warnings.is_empty() {
            self.notify_error_sticky(format!(
                "session loaded with unavailable attachments: {}",
                degraded_warnings.join("; ")
            ));
        }
        self.finish_transcript_turn();
        self.transcript_win_mut().follow_tail();
    }

    /// Resolve a Confirm dialog. Cancels the active turn when the choice requires it.
    pub(crate) fn handle_confirm_resolve(
        &mut self,
        choice: ConfirmChoice,
        message: Option<String>,
        request_id: u64,
        call_id: &str,
        tool_name: &str,
    ) {
        let should_cancel = self.resolve_confirm((choice, message), call_id, request_id, tool_name);
        if should_cancel {
            self.finish_turn(crate::app::TurnEnd::Cancelled);
            self.agent = None;
        }
    }
}
