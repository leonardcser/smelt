use crate::app::TuiApp;
use smelt_core::session;
use smelt_core::working::{TurnOutcome, TurnPhase};
use smelt_core::{Block, ToolOutput, ToolState, ToolStatus};

use protocol::{AgentMode, Content, Message, Role, UiCommand};
use std::collections::HashMap;
use std::time::Duration;

impl TuiApp {
    /// Redact secrets from user-submitted text before it lands on screen or
    /// in history. The `display` string is the rendered form of the submitted
    /// message; `content` is what gets sent to the engine. Both are scrubbed
    /// so the UI and the LLM see the same redacted form.
    pub(crate) fn redact_user_submission(&self, content: &mut Content, display: &mut String) {
        if self.core.config.settings.redact_secrets {
            engine::redact::redact_content(content);
            *display = engine::redact::redact(display);
        }
    }

    pub(crate) fn set_history(&mut self, messages: Vec<Message>) {
        self.core.session.messages = messages;
        self.sync_session_snapshot();
        let count = self.core.session.messages.len();
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

    /// Resolve the active model to its full `provider/model` key so that
    /// resuming a session restores the same provider (auth method), even
    /// when the same model name is configured under multiple providers.
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

    /// Record current token count and cost so they can be restored on rewind.
    pub(crate) fn snapshot_tokens(&mut self) {
        if let Some(tokens) = self.core.session.context_tokens {
            self.core
                .session
                .token_snapshots
                .push((self.core.session.messages.len(), tokens));
        }
        let cost = self.core.session.session_cost_usd;
        self.core
            .session
            .cost_snapshots
            .push((self.core.session.messages.len(), cost));
    }

    pub(crate) fn fork_session(&mut self) {
        if self.core.session.messages.is_empty() {
            self.notify_error("nothing to fork".into());
            return;
        }
        self.save_session();
        self.flush_persist();
        let original_id = self.core.session.id.clone();
        let forked = self.core.session.fork();
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
                count: self.core.session.messages.len(),
            }),
        );
        self.notify(format!("forked from {original_id}"));
    }

    pub(crate) fn reset_session(&mut self) {
        // Cancel any in-flight engine work (agent turn, title generation, etc.)
        // before clearing state so stale events don't restore old data.
        self.core.engine.send(UiCommand::Cancel);
        let old_id = self.core.session.id.clone();
        self.core.session.messages.clear();
        self.reset_session_permissions();
        self.queued_messages.clear();
        self.task_label = None;
        self.working.clear();
        self.input.win.scroll_top = 0;
        if let Some(w) = self.ui.win_mut(crate::ui::PROMPT_WIN) {
            w.viewport = None;
        }
        if let Some(w) = self.ui.win_mut(crate::ui::TRANSCRIPT_WIN) {
            w.viewport = None;
        }
        self.clear_transcript();
        self.app_focus = crate::app::AppFocus::Prompt;
        self.input.clear();
        self.input.store.clear();
        self.core.processes.clear();
        self.core.session = session::Session::new();
        self.pending_title = false;
        self.compact_epoch += 1;
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
        // Drain stale engine events so old Messages snapshots don't
        // restore history into the freshly cleared session.
        while self.core.engine.try_recv().is_ok() {}
    }

    pub fn load_session(&mut self, loaded: session::Session) {
        let old_id = self.core.session.id.clone();
        self.flush_persist();

        // Restore per-session settings through the canonical helpers so
        // state.json + engine + screen all stay in sync with `self`.
        if let Some(mode) = loaded.mode.as_deref().and_then(AgentMode::parse) {
            self.set_mode(mode);
        }
        if let Some(effort) = loaded.reasoning_effort {
            self.set_reasoning_effort(effort);
        }
        // Only restore model/API settings if not overridden by CLI.
        if !self.core.config.cli_model_override
            && !self.core.config.cli_api_base_override
            && !self.core.config.cli_api_key_env_override
        {
            if let Some(ref model_key) = loaded.model {
                // Prefer an exact key match so the original provider/auth method
                // is restored. Fall back to a unique bare model name for
                // sessions saved before the key was persisted.
                let resolved_key = smelt_core::config::resolve_model_ref(
                    &self.core.config.available_models,
                    model_key,
                )
                .ok()
                .map(|resolved| resolved.key.clone());
                if let Some(key) = resolved_key {
                    self.apply_model(&key);
                }
            }
        }

        self.core.session = loaded;
        if let Some(ref slug) = self.core.session.slug {
            self.set_task_label(slug.clone());
        }
        // Defensive scrub: drop any snapshots beyond restored history.
        let hist_len = self.core.session.messages.len();
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
        self.input.clear();
        self.input.store.clear();
        self.pending_title = false;
        self.core.processes.clear();
        self.compact_epoch += 1;
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
                count: self.core.session.messages.len(),
            }),
        );
        // Drain stale engine events so old snapshots don't overwrite
        // the loaded session's state.
        while self.core.engine.try_recv().is_ok() {}
    }

    // ── History / session ────────────────────────────────────────────────

    /// Rebuild the screen from session history and import persisted render cache.
    pub(crate) fn restore_screen(&mut self) {
        self.rebuild_screen_from_history();
    }

    fn rebuild_screen_from_history(&mut self) {
        self.clear_transcript();
        if let Some(ref slug) = self.core.session.slug {
            self.set_task_label(slug.clone());
        }
        if self.core.session.messages.is_empty() {
            return;
        }

        let mut tool_outputs: HashMap<String, ToolOutput> = HashMap::new();
        let mut tool_elapsed: HashMap<String, u64> = HashMap::new();
        let render_cache = session::load_render_cache(&self.core.session);
        for msg in &self.core.session.messages {
            if matches!(msg.role, Role::Tool) {
                if let Some(ref id) = msg.tool_call_id {
                    let text = msg
                        .content
                        .as_ref()
                        .map(|c| c.text_content())
                        .unwrap_or_default();
                    tool_outputs.insert(
                        id.clone(),
                        ToolOutput {
                            content: text,
                            is_error: msg.is_error,
                            metadata: None,
                            render_cache: None,
                        },
                    );
                }
            }
        }
        if let Some(cache) = render_cache.as_ref() {
            for (call_id, output) in &mut tool_outputs {
                output.render_cache = cache.get_tool_output(call_id).cloned();
            }
        }

        for (_, meta) in &self.core.session.turn_metas {
            tool_elapsed.extend(meta.tool_elapsed.iter().map(|(k, v)| (k.clone(), *v)));
        }

        let messages = self.core.session.messages.clone();
        for msg in &messages {
            match msg.role {
                Role::User => {
                    if let Some(ref content) = msg.content {
                        let text = content.text_content();
                        let prefix_marker = engine::SUMMARY_PREFIX.trim_end();
                        if let Some(rest) = text.strip_prefix(prefix_marker) {
                            let summary = rest.trim_start_matches('\n');
                            self.push_block(Block::Compacted {
                                summary: summary.to_string(),
                            });
                        } else {
                            let image_labels = content.image_labels();
                            let display_text = if image_labels.is_empty() {
                                text
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
                    }
                }
                Role::Assistant => {
                    if let Some(ref reasoning) = msg.reasoning_content {
                        if !reasoning.is_empty() {
                            self.push_block(Block::Thinking {
                                content: reasoning.clone(),
                            });
                        }
                    }
                    if let Some(ref content) = msg.content {
                        self.push_block(Block::Text {
                            content: content.text_content(),
                        });
                    }
                    if let Some(ref calls) = msg.tool_calls {
                        for tc in calls {
                            let args: HashMap<String, serde_json::Value> =
                                serde_json::from_str(&tc.function.arguments).unwrap_or_default();
                            let output = tool_outputs.get(&tc.id).cloned().map(|mut out| {
                                out.render_cache = render_cache
                                    .as_ref()
                                    .and_then(|cache| cache.get_tool_output(&tc.id).cloned());
                                out
                            });

                            let status = if let Some(ref out) = output {
                                if out.content.contains("denied this tool call")
                                    || out.content.contains("blocked this tool call")
                                {
                                    ToolStatus::Denied
                                } else if out.is_error {
                                    ToolStatus::Err
                                } else {
                                    ToolStatus::Ok
                                }
                            } else {
                                ToolStatus::Pending
                            };
                            let elapsed = tool_elapsed
                                .get(&tc.id)
                                .map(|ms| Duration::from_millis(*ms));
                            let summary = self.lua.tool_summary(&tc.function.name, &args);
                            self.push_tool_call(
                                Block::ToolCall {
                                    call_id: tc.id.clone(),
                                    name: tc.function.name.clone(),
                                    summary,
                                    args,
                                },
                                ToolState {
                                    status,
                                    elapsed,
                                    output: output.map(Box::new),
                                    user_message: None,
                                },
                            );
                        }
                    }
                }
                Role::Tool => {}
                Role::System => {}
            }
        }

        if let Some((_, meta)) = self.core.session.turn_metas.last() {
            self.working.restore_from_turn_meta(meta);
        }

        // Reattach the persisted layout cache, if any. Must happen *after*
        // every block has been pushed so the cache vector lengths match.
        // Per-block width validity is enforced inside `import_layout_cache`.
        if let Some(layout_cache) = session::load_layout_cache(&self.core.session) {
            self.import_layout_cache(layout_cache);
        }
    }

    pub(crate) fn save_session(&mut self) {
        let _perf = smelt_core::perf::begin("session:save");
        if self.core.session.messages.is_empty() {
            return;
        }
        self.sync_session_snapshot();
        // Skip persisting render/layout caches when redaction is enabled —
        // they contain raw source text from tool output that would leak secrets.
        let (render_cache, layout_cache) = if self.core.config.settings.redact_secrets {
            (None, None)
        } else {
            (
                self.export_render_cache(),
                self.layout_cache_dirty()
                    .then(|| self.export_layout_cache())
                    .flatten(),
            )
        };
        let blobs = self
            .input
            .store
            .image_blobs()
            .into_iter()
            .map(|(filename, data_url)| crate::persist::Blob { filename, data_url })
            .collect();
        self.persister.save(crate::persist::PersistRequest {
            session: self.core.session.clone(),
            blobs,
            render_cache,
            layout_cache,
        });
    }

    /// Block until all queued persist writes have completed. Call before
    /// code paths that read session files off disk (load, fork, shutdown).
    pub(crate) fn flush_persist(&self) {
        self.persister.flush();
    }

    pub(crate) fn maybe_generate_title(&mut self, current_message: Option<&str>) {
        if self.pending_title {
            engine::log::entry(
                engine::log::Level::Debug,
                "title_skip",
                &serde_json::json!({"reason": "pending"}),
            );
            return;
        }
        let last_user_idx = self
            .core
            .session
            .messages
            .iter()
            .rposition(|m| matches!(m.role, protocol::Role::User));
        let last_user_message = match (last_user_idx, current_message) {
            (_, Some(msg)) if !msg.is_empty() => msg.to_string(),
            (Some(i), _) => self
                .core
                .session
                .messages
                .get(i)
                .and_then(|m| m.content.as_ref())
                .map(|c| c.text_content())
                .unwrap_or_default(),
            _ => String::new(),
        };
        if last_user_message.is_empty() {
            engine::log::entry(
                engine::log::Level::Debug,
                "title_skip",
                &serde_json::json!({"reason": "no_user_messages"}),
            );
            return;
        }
        // Tail of assistant text after the last user message (bounded to 1000 chars).
        let tail_start = last_user_idx.map(|i| i + 1).unwrap_or(0);
        let mut assistant_tail: String = self.core.session.messages[tail_start..]
            .iter()
            .filter(|m| matches!(m.role, protocol::Role::Assistant))
            .filter_map(|m| m.content.as_ref().map(|c| c.text_content()))
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if assistant_tail.len() > 1000 {
            let cut = assistant_tail.len() - 1000;
            let boundary = assistant_tail.ceil_char_boundary(cut);
            assistant_tail = assistant_tail[boundary..].to_string();
        }
        engine::log::entry(
            engine::log::Level::Info,
            "title_generate",
            &serde_json::json!({
                "user_chars": last_user_message.len(),
                "assistant_chars": assistant_tail.len(),
                "current_title": self.core.session.title,
            }),
        );
        self.pending_title = true;
        self.core.engine.send(UiCommand::GenerateTitle {
            last_user_message,
            assistant_tail,
        });
    }

    pub(crate) fn is_compacting(&self) -> bool {
        self.working.is_compacting()
    }

    pub(crate) fn compact_history(&mut self, instructions: Option<String>) {
        self.pending_compact_epoch = self.compact_epoch;
        {
            self.working.begin(TurnPhase::Compacting);
        };
        self.core.engine.send(UiCommand::Compact {
            history: self.core.session.messages.clone(),
            instructions,
        });
    }

    pub(crate) fn apply_compaction(&mut self, messages: Vec<protocol::Message>) {
        if messages.is_empty() {
            {
                self.working.finish(TurnOutcome::Done);
            };
            return;
        }

        // Replace history with the compacted messages (summary + kept turns).
        // Old snapshots key into pre-compaction positions and are no longer
        // valid, but the running cost carries forward.
        self.core.session.messages = messages;
        self.core.session.token_snapshots.clear();
        self.core.session.cost_snapshots.clear();
        self.core.session.turn_metas.clear();
        self.core.session.context_tokens = None;

        self.restore_screen();
        self.save_session();
        {
            self.working.finish(TurnOutcome::Done);
        };
        self.transcript_window.scroll_to_bottom();
    }

    pub(crate) fn maybe_auto_compact(&mut self) {
        if !self.core.config.settings.auto_compact {
            return;
        }
        let Some(ctx) = self.core.config.context_window else {
            return;
        };
        let Some(tokens) = self.core.session.context_tokens else {
            return;
        };
        if tokens as u64 * 100 >= ctx as u64 * engine::compact_threshold_percent() {
            self.compact_history(None);
        }
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
        for (i, msg) in self.core.session.messages.iter().enumerate() {
            if matches!(msg.role, Role::User) {
                user_count += 1;
                if user_count > user_turns_to_keep {
                    hist_idx = i;
                    break;
                }
            }
            hist_idx = i + 1;
        }

        // Extract image (label, data_url) pairs from the target message before truncating.
        let images: Vec<(String, String)> = self
            .core
            .session
            .messages
            .get(hist_idx)
            .and_then(|msg| msg.content.as_ref())
            .map(|content| match content {
                Content::Parts(parts) => parts
                    .iter()
                    .filter_map(|p| match p {
                        protocol::ContentPart::ImageUrl { url, label } => {
                            Some((label.clone().unwrap_or_else(|| "image".into()), url.clone()))
                        }
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            })
            .unwrap_or_default();

        self.core.session.messages.truncate(hist_idx);
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
        self.compact_epoch += 1;

        turn_text.map(|t| (t, images))
    }

    // ── Agent internals ──────────────────────────────────────────────────

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
