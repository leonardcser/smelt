use crate::app::TuiApp;
use smelt_core::session;
use smelt_core::{Block, ToolOutput, ToolState, ToolStatus};

use protocol::{AgentMode, AssistantTurn, Content, HistoryItem, UiCommand};
use std::collections::HashMap;
use std::time::Duration;

impl TuiApp {
    /// Redact secrets from user-submitted text before it lands on screen or
    /// in history. The `display` string is the rendered form of the submitted
    /// message; `content` is what gets sent to the engine. Both are scrubbed
    /// so the UI and the LLM see the same redacted form.
    pub(crate) fn redact_user_submission(&self, content: &mut Content, display: &mut String) {
        if self.core.config.settings.redact_secrets {
            let _perf = smelt_perf::perf::begin("ingress:redact");
            engine::redact::redact_content(content);
            *display = engine::redact::redact(display);
        }
    }

    pub(crate) fn set_history(&mut self, history: Vec<HistoryItem>) {
        self.core.session.history = history;
        self.sync_session_snapshot();
        let count = self.core.session.history.len();
        self.core.cells.set_dyn(
            "history",
            std::rc::Rc::new(smelt_core::cells::HistoryDelta {
                kind: "set".into(),
                count,
            }),
        );
    }

    pub(crate) fn sync_session_snapshot(&mut self) {
        self.core.session.updated_at_ms = session::now_ms();
        self.core.session.mode = Some(self.core.config.mode.as_str().to_string());
        self.core.session.reasoning_effort = Some(self.core.config.reasoning_effort);
        self.core.session.model = Some(self.current_model_key());
        if let Ok(mut guard) = self.shared_session.lock() {
            *guard = Some(self.core.session.clone());
        }
    }

    /// Full `provider/model` key so resuming a session restores the correct provider/auth.
    fn current_model_key(&self) -> String {
        self.core
            .config
            .available_models
            .iter()
            .find(|m| {
                m.model_name == self.core.config.model
                    && m.api_base == self.core.config.api_base
                    && m.api_key_env == self.core.config.api_key_env
                    && m.provider_type == self.core.config.provider_type
            })
            .map(|m| m.key.clone())
            .unwrap_or_else(|| self.core.config.model.clone())
    }

    pub(crate) fn snapshot_tokens(&mut self) {
        if let Some(tokens) = self.core.session.context_tokens {
            self.core
                .session
                .token_snapshots
                .push((self.core.session.history.len(), tokens));
        }
        let cost = self.core.session.session_cost_usd;
        self.core
            .session
            .cost_snapshots
            .push((self.core.session.history.len(), cost));
    }

    pub(crate) fn fork_session(&mut self) {
        if self.core.session.history.is_empty() {
            self.notify_error("nothing to fork".into());
            return;
        }
        self.save_session();
        self.flush_persist();
        let original_id = self.core.session.id.clone();
        let forked = self.core.session.fork(self.core.env.pid());
        self.core.session = forked;
        self.save_session();
        self.flush_persist();
        self.core
            .cells
            .set_dyn("session_ended", std::rc::Rc::new(original_id.clone()));
        self.core.cells.set_dyn(
            "session_started",
            std::rc::Rc::new(self.core.session.id.clone()),
        );
        self.core.cells.set_dyn(
            "history",
            std::rc::Rc::new(smelt_core::cells::HistoryDelta {
                kind: "forked".into(),
                count: self.core.session.history.len(),
            }),
        );
        self.notify(format!("forked from {original_id}"));
    }

    pub(crate) fn reset_session(&mut self) {
        let _perf = smelt_perf::perf::begin("app:reset_session");
        // Cancel in-flight engine work before clearing state so stale events don't restore old data.
        self.core.engine.send(UiCommand::Cancel);
        let old_id = self.core.session.id.clone();
        self.core.session.history.clear();
        self.reset_session_permissions();
        self.queued_messages.clear();
        self.task_label = None;
        self.working.clear();
        if let Some(w) = self.ui.win_mut(crate::app::PROMPT_WIN) {
            w.scroll_top = 0;
            w.viewport = None;
        }
        if let Some(w) = self.ui.win_mut(crate::app::TRANSCRIPT_WIN) {
            w.viewport = None;
        }
        self.clear_transcript();
        self.app_focus = crate::app::AppFocus::Prompt;
        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
        self.input.clear(&mut pctx);
        self.input.store.lock().unwrap().clear();
        self.core.processes.clear();
        self.core.session = session::Session::new(self.core.env.pid(), self.core.env.cwd());
        if let Ok(mut guard) = self.shared_session.lock() {
            *guard = None;
        }
        self.core
            .cells
            .set_dyn("session_ended", std::rc::Rc::new(old_id));
        self.core.cells.set_dyn(
            "session_started",
            std::rc::Rc::new(self.core.session.id.clone()),
        );
        self.core.cells.set_dyn(
            "history",
            std::rc::Rc::new(smelt_core::cells::HistoryDelta {
                kind: "cleared".into(),
                count: 0,
            }),
        );
        // Drain stale events so old Messages snapshots don't restore history into the fresh session.
        while self.core.engine.try_recv().is_ok() {}
    }

    pub fn load_session(&mut self, loaded: session::Session) {
        let old_id = self.core.session.id.clone();
        self.flush_persist();

        if let Some(mode) = loaded.mode.as_deref().and_then(AgentMode::parse) {
            self.set_mode(mode, false);
        }
        if let Some(effort) = loaded.reasoning_effort {
            self.set_reasoning_effort(effort, false);
        }
        // Only restore model/API settings if not overridden by CLI.
        if !self.core.config.cli_model_override
            && !self.core.config.cli_api_base_override
            && !self.core.config.cli_api_key_env_override
        {
            if let Some(ref model_key) = loaded.model {
                // Prefer exact key match; fall back to bare model name for older sessions.
                let resolved_key = smelt_core::config::resolve_model_ref(
                    &self.core.config.available_models,
                    model_key,
                )
                .ok()
                .map(|resolved| resolved.key.clone());
                if let Some(key) = resolved_key {
                    self.apply_model(&key, false);
                }
            }
        }

        self.core.session = loaded;
        if let Some(ref slug) = self.core.session.slug {
            self.set_task_label(slug.clone());
        }
        // Drop snapshots beyond the restored history length.
        let hist_len = self.core.session.history.len();
        self.core
            .session
            .token_snapshots
            .retain(|(len, _)| *len <= hist_len);
        self.core
            .session
            .cost_snapshots
            .retain(|(len, _)| *len <= hist_len);
        self.core.session.session_cost_usd = self
            .core
            .session
            .cost_snapshots
            .last()
            .map(|&(_, c)| c)
            .unwrap_or(0.0);
        self.reset_session_permissions();
        self.queued_messages.clear();
        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
        self.input.clear(&mut pctx);
        self.input.store.lock().unwrap().clear();
        self.core.processes.clear();
        self.sync_session_snapshot();
        self.core
            .cells
            .set_dyn("session_ended", std::rc::Rc::new(old_id));
        self.core.cells.set_dyn(
            "session_started",
            std::rc::Rc::new(self.core.session.id.clone()),
        );
        self.core.cells.set_dyn(
            "history",
            std::rc::Rc::new(smelt_core::cells::HistoryDelta {
                kind: "loaded".into(),
                count: self.core.session.history.len(),
            }),
        );
        // Drain stale engine events so old snapshots don't overwrite
        // the loaded session's state.
        while self.core.engine.try_recv().is_ok() {}
    }

    // ── History / session ────────────────────────────────────────────────

    pub(crate) fn restore_screen(&mut self) {
        self.rebuild_screen_from_history();
    }

    fn rebuild_screen_from_history(&mut self) {
        self.clear_transcript();
        if let Some(ref slug) = self.core.session.slug {
            self.set_task_label(slug.clone());
        }
        if self.core.session.history.is_empty() {
            return;
        }

        // Per-call elapsed times survive across reloads via turn_metas.
        // ToolInvocation also carries its own elapsed; we prefer the
        // in-line value and fall back to turn_metas for older sessions.
        let mut tool_elapsed: HashMap<String, u64> = HashMap::new();
        for (_, meta) in &self.core.session.turn_metas {
            tool_elapsed.extend(meta.tool_elapsed.iter().map(|(k, v)| (k.clone(), *v)));
        }

        let history = self.core.session.history.clone();
        for item in &history {
            match item {
                HistoryItem::User { content } => self.push_user_block(content),
                HistoryItem::Assistant(turn) => self.push_assistant_blocks(turn, &tool_elapsed),
                HistoryItem::System { .. } => {}
            }
        }

        if let Some((_, meta)) = self.core.session.turn_metas.last() {
            self.working.restore_from_turn_meta(meta);
        }
    }

    fn push_user_block(&mut self, content: &Content) {
        let text = content.text_content();
        let prefix_marker = engine::SUMMARY_PREFIX.trim_end();
        if let Some(rest) = text.strip_prefix(prefix_marker) {
            let summary = rest.trim_start_matches('\n');
            self.push_block(Block::Compacted {
                summary: summary.to_string(),
            });
            return;
        }
        let image_labels = content.image_labels();
        let display_text = if image_labels.is_empty() {
            text.into_owned()
        } else {
            let suffix = image_labels.join(" ");
            if text.is_empty() {
                suffix
            } else {
                format!("{text} {suffix}")
            }
        };
        self.push_block(Block::User {
            text: display_text,
            image_labels,
        });
    }

    fn push_assistant_blocks(&mut self, turn: &AssistantTurn, tool_elapsed: &HashMap<String, u64>) {
        if let Some(ref reasoning) = turn.reasoning {
            if !reasoning.is_empty() {
                self.push_block(Block::Thinking {
                    content: reasoning.clone(),
                });
            }
        }
        if let Some(ref content) = turn.content {
            self.push_block(Block::Text {
                content: content.text_content().into_owned(),
            });
        }
        for inv in &turn.invocations {
            let args: HashMap<String, serde_json::Value> =
                serde_json::from_str(&inv.arguments).unwrap_or_default();
            let status = if inv.result.content.contains("denied this tool call")
                || inv.result.content.contains("blocked this tool call")
            {
                ToolStatus::Denied
            } else if inv.result.is_error {
                ToolStatus::Err
            } else {
                ToolStatus::Ok
            };
            let output = ToolOutput {
                content: inv.result.content.clone(),
                is_error: inv.result.is_error,
                metadata: inv.result.metadata.clone(),
            };
            let elapsed_ms = inv
                .elapsed_ms
                .or_else(|| tool_elapsed.get(&inv.call_id).copied());
            let summary = self.lua.tool_summary(&inv.name, &args);
            self.push_tool_call(
                Block::ToolCall {
                    call_id: inv.call_id.clone(),
                    name: inv.name.clone(),
                    summary,
                    args,
                },
                ToolState {
                    status,
                    elapsed: elapsed_ms.map(Duration::from_millis),
                    output: Some(Box::new(output)),
                    user_message: None,
                    render_cache: None,
                    layout_revision: 0,
                },
            );
        }
    }

    pub(crate) fn save_session(&mut self) {
        let _perf = smelt_perf::perf::begin("session:save");
        if self.core.session.history.is_empty() {
            return;
        }
        self.sync_session_snapshot();
        let blobs = self
            .input
            .store
            .lock()
            .unwrap()
            .image_blobs()
            .into_iter()
            .map(|(filename, data_url)| crate::persist::Blob { filename, data_url })
            .collect();
        self.persister.save(crate::persist::PersistRequest {
            session: self.core.session.clone(),
            blobs,
        });
    }

    /// Block until all queued persist writes complete. Call before reading session files from disk.
    pub(crate) fn flush_persist(&self) {
        self.persister.flush();
    }

    /// Atomically replace `session.messages` with `messages`. Clears token /
    /// cost / turn-meta snapshots (they key into pre-replacement positions),
    /// resets `context_tokens`, repaints the screen, and saves the session.
    /// No-op when `messages` is empty.
    pub(crate) fn replace_history(&mut self, history: Vec<HistoryItem>) {
        if history.is_empty() {
            return;
        }
        self.core.session.history = history;
        self.core.session.token_snapshots.clear();
        self.core.session.cost_snapshots.clear();
        self.core.session.turn_metas.clear();
        self.core.session.context_tokens = None;

        self.restore_screen();
        self.save_session();
        self.transcript_win_mut().scroll_to_bottom();
    }

    pub(crate) fn rewind_to(
        &mut self,
        block_idx: usize,
    ) -> Option<(String, Vec<(String, String)>)> {
        let turns = self.user_turns();
        let turn_text = turns
            .iter()
            .find(|(i, _)| *i == block_idx)
            .map(|(_, t)| t.clone());
        let user_turns_to_keep = turns.iter().filter(|(i, _)| *i < block_idx).count();

        let mut user_count = 0;
        let mut hist_idx = 0;
        for (i, item) in self.core.session.history.iter().enumerate() {
            if matches!(item, HistoryItem::User { .. }) {
                user_count += 1;
                if user_count > user_turns_to_keep {
                    hist_idx = i;
                    break;
                }
            }
            hist_idx = i + 1;
        }

        let images: Vec<(String, String)> = match self.core.session.history.get(hist_idx) {
            Some(HistoryItem::User {
                content: Content::Parts(parts),
            }) => parts
                .iter()
                .filter_map(|p| match p {
                    protocol::ContentPart::ImageUrl { url, label } => {
                        Some((label.clone().unwrap_or_else(|| "image".into()), url.clone()))
                    }
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };

        self.core.session.history.truncate(hist_idx);
        truncate_keyed(&mut self.core.session.token_snapshots, hist_idx);
        truncate_keyed(&mut self.core.session.cost_snapshots, hist_idx);
        truncate_keyed(&mut self.core.session.turn_metas, hist_idx);
        self.core.session.session_cost_usd = self
            .core
            .session
            .cost_snapshots
            .last()
            .map(|&(_, c)| c)
            .unwrap_or(0.0);
        self.core.session.context_tokens =
            self.core.session.token_snapshots.last().map(|&(_, t)| t);
        self.truncate_to(block_idx);
        self.reset_session_permissions();

        turn_text.map(|t| (t, images))
    }

    pub(crate) fn show_user_message(&mut self, input: &str, image_labels: Vec<String>) {
        self.push_block(Block::User {
            text: input.to_string(),
            image_labels,
        });
    }
}

/// Drop entries whose history-length key exceeds `hist_idx`.
fn truncate_keyed<T>(snapshots: &mut Vec<(usize, T)>, hist_idx: usize) {
    while snapshots.last().is_some_and(|(len, _)| *len > hist_idx) {
        snapshots.pop();
    }
}
