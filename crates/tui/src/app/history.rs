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
        let applied_notes: Vec<String> = self
            .pending_history_appends
            .iter()
            .filter(|pending| {
                history.iter().any(|item| match item {
                    HistoryItem::User { content } => content.as_text() == pending.history_note(),
                    _ => false,
                })
            })
            .map(|pending| pending.history_note().to_string())
            .collect();
        self.core
            .session
            .merge_model_history_snapshot(engine::SUMMARY_PREFIX, history);
        for note in applied_notes {
            self.commit_pending_history_append(&note);
        }
        self.sync_session_snapshot();
        self.publish_history_delta("set");
    }

    pub(crate) fn publish_history_delta(&mut self, kind: &str) {
        let count = self.core.session.history.len();
        self.core.cells.set_dyn(
            "history",
            std::rc::Rc::new(smelt_core::cells::HistoryDelta {
                kind: kind.into(),
                count,
            }),
        );
    }

    pub(crate) fn apply_history_append_to_history(
        &mut self,
        note: &str,
        replace_user_prefix: Option<&str>,
    ) {
        let new_item = HistoryItem::user(Content::text(note.to_string()));
        if let Some(prefix) = replace_user_prefix {
            let last_matches = self
                .core
                .session
                .history
                .last()
                .and_then(|item| match item {
                    HistoryItem::User { content } => Some(content),
                    _ => None,
                })
                .is_some_and(|c| c.as_text().starts_with(prefix));
            if last_matches {
                if let Some(last) = self.core.session.history.last_mut() {
                    *last = new_item;
                }
                return;
            }
        }
        self.core.session.history.push(new_item);
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
        // Keep the last provider baseline when this turn produced no fresh
        // usage. It is keyed by `context_tokens_history_len`, so request
        // preparation can add a local delta for appended history instead of
        // losing the authoritative count after a cancel or failed compaction.
        // Successful checkpoints and full history replacements clear the
        // baseline explicitly because they change the model-visible prefix.
        self.context_tokens_updated_this_turn = false;
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
        // Cancel any in-flight turn and Lua tasks before swapping sessions.
        if self.agent.is_some() {
            self.cancel_agent();
            self.agent = None;
        }
        self.save_session();
        self.flush_persist();
        self.stop_background_processes();
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
        self.publish_history_delta("forked");
        self.notify(format!("forked from {original_id}"));
        // Drain stale events so old snapshots don't overwrite the forked session.
        while self.core.engine.try_recv().is_ok() {}
    }

    pub(crate) fn reset_session(&mut self) {
        let _perf = smelt_perf::perf::begin("app:reset_session");
        // Cancel in-flight engine work and Lua tasks before clearing state so
        // stale events and running child processes don't restore old data.
        if self.agent.is_some() {
            self.cancel_agent();
            self.agent = None;
        } else {
            self.core.engine.send(UiCommand::Cancel);
            self.lua.cancel_tasks();
        }
        let old_id = self.core.session.id.clone();
        self.core.session.history.clear();
        self.reset_session_permissions();
        self.queued_inputs.clear();
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
        self.stop_background_processes();
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
        self.publish_history_delta("cleared");
        // Drain stale events so old Messages snapshots don't restore history into the fresh session.
        while self.core.engine.try_recv().is_ok() {}
    }

    pub fn load_session(&mut self, loaded: session::Session) {
        // Cancel any in-flight turn and Lua tasks before swapping sessions.
        if self.agent.is_some() {
            self.cancel_agent();
            self.agent = None;
        }
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
        self.queued_inputs.clear();
        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
        self.input.clear(&mut pctx);
        self.input.store.lock().unwrap().clear();
        self.stop_background_processes();
        self.sync_session_snapshot();
        self.core
            .cells
            .set_dyn("session_ended", std::rc::Rc::new(old_id));
        self.core.cells.set_dyn(
            "session_started",
            std::rc::Rc::new(self.core.session.id.clone()),
        );
        self.publish_history_delta("loaded");
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
        let compaction = self.core.session.checkpoint.clone();
        for (idx, item) in history.iter().enumerate() {
            if compaction
                .as_ref()
                .is_some_and(|cp| cp.first_live_index == idx)
            {
                self.push_block(Block::Compacted {
                    summary: compaction
                        .as_ref()
                        .map(|cp| cp.summary.clone())
                        .unwrap_or_default(),
                });
            }
            match item {
                HistoryItem::User { content } => self.push_user_block(content),
                HistoryItem::Assistant(turn) => self.push_assistant_blocks(turn, &tool_elapsed),
                HistoryItem::System { .. } => {}
            }
        }
        if compaction
            .as_ref()
            .is_some_and(|cp| cp.first_live_index >= history.len())
        {
            self.push_block(Block::Compacted {
                summary: compaction.map(|cp| cp.summary).unwrap_or_default(),
            });
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
        if let Some(note) = text.strip_prefix(protocol::MODE_NOTE_PREFIX) {
            self.push_block(self.lua.mode_block(None, note.trim()));
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
        self.core.session.checkpoint = None;
        self.core.session.cost_snapshots.clear();
        self.core.session.turn_metas.clear();
        self.core.session.clear_context_tokens();

        self.restore_screen();
        self.save_session();
        self.transcript_win_mut().scroll_to_bottom();
    }

    pub(crate) fn install_context_checkpoint(
        &mut self,
        kind: String,
        summary: String,
        first_live_message_index: usize,
        tokens_before: Option<u32>,
    ) -> bool {
        let installed = self.core.session.install_context_checkpoint(
            kind,
            summary,
            first_live_message_index,
            tokens_before,
        );
        if !installed {
            self.notify("nothing old enough to compact".to_string());
            return false;
        }
        self.restore_screen();
        self.save_session();
        self.transcript_win_mut().scroll_to_bottom();
        true
    }

    pub(crate) fn model_history(&self) -> Vec<HistoryItem> {
        self.core.session.model_history(engine::SUMMARY_PREFIX)
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
        let keep_checkpoint_at_boundary = turn_text.is_some()
            && self
                .core
                .session
                .checkpoint
                .as_ref()
                .is_some_and(|cp| cp.first_live_index == hist_idx);
        if !keep_checkpoint_at_boundary {
            self.core.session.clear_checkpoint_if_rewound_to(hist_idx);
        }
        truncate_keyed(&mut self.core.session.cost_snapshots, hist_idx);
        truncate_keyed(&mut self.core.session.turn_metas, hist_idx);
        self.core.session.session_cost_usd = self
            .core
            .session
            .cost_snapshots
            .last()
            .map(|&(_, c)| c)
            .unwrap_or(0.0);
        self.truncate_to(block_idx);
        self.reset_session_permissions();
        if self.core.session.history.is_empty() {
            self.task_label = None;
        }
        self.sync_session_snapshot();
        self.publish_history_delta("rewound");

        turn_text.map(|t| (t, images))
    }

    pub(crate) fn rewind_to_start(&mut self) {
        self.core.session.history.clear();
        self.core.session.checkpoint = None;
        self.core.session.cost_snapshots.clear();
        self.core.session.turn_metas.clear();
        self.core.session.session_cost_usd = 0.0;
        self.core.session.clear_context_tokens();
        self.task_label = None;
        self.clear_transcript();
        self.reset_session_permissions();
        self.sync_session_snapshot();
        self.publish_history_delta("rewound");
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

#[cfg(test)]
mod checkpoint_tests {
    use super::*;
    use protocol::Content;
    use smelt_core::ContextCheckpoint;

    fn user(text: &str) -> HistoryItem {
        HistoryItem::user(Content::text(text))
    }

    fn assistant(text: &str) -> HistoryItem {
        HistoryItem::Assistant(protocol::AssistantTurn::terminal(
            Some(Content::text(text)),
            None,
            Vec::new(),
        ))
    }

    fn is_compaction_summary_item(item: &HistoryItem) -> bool {
        smelt_core::session::is_context_checkpoint_summary(item, engine::SUMMARY_PREFIX)
    }

    fn add_background_process(app: &mut crate::app::test_harness::TestApp) -> String {
        let child = smelt_core::process::spawn_shell_child(
            "sleep 30",
            &smelt_core::process::ShellSpec::default(),
        )
        .expect("spawn background child");
        let id = app.app.core.processes.child_id(&child);
        app.app
            .core
            .processes
            .spawn(id.clone(), "sleep 30", child, std::time::Instant::now());
        id
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reset_session_stops_background_processes() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        add_background_process(&mut app);
        assert_eq!(app.app.core.processes.running_count(), 1);

        app.app.reset_session();

        assert_eq!(app.app.core.processes.running_count(), 0);
        assert!(app.app.core.processes.list().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn load_session_stops_background_processes() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        add_background_process(&mut app);
        assert_eq!(app.app.core.processes.running_count(), 1);

        let loaded = smelt_core::session::Session::new(99, std::path::PathBuf::from("/tmp/loaded"));
        app.app.load_session(loaded);

        assert_eq!(app.app.core.processes.running_count(), 0);
        assert!(app.app.core.processes.list().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fork_session_stops_background_processes() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.core.session.history = vec![user("hello")];
        add_background_process(&mut app);
        assert_eq!(app.app.core.processes.running_count(), 1);

        app.app.fork_session();

        assert_eq!(app.app.core.processes.running_count(), 0);
        assert!(app.app.core.processes.list().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn quit_cleanup_stops_background_processes() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        add_background_process(&mut app);
        assert_eq!(app.app.core.processes.running_count(), 1);

        app.app.stop_background_processes();

        assert_eq!(app.app.core.processes.running_count(), 0);
        assert!(app.app.core.processes.list().is_empty());
    }

    #[test]
    fn model_history_without_checkpoint_returns_full_history() {
        let session = smelt_core::session::Session::new(1, std::path::PathBuf::from("/tmp"));
        assert!(session.checkpoint.is_none());
        // model_history is the same as history when no checkpoint
        let history = vec![user("hello"), assistant("world")];
        let mut s = session;
        s.history = history.clone();
        assert_eq!(s.model_history("prefix").len(), history.len());
    }

    #[test]
    fn model_history_with_checkpoint_prepends_summary_and_tail() {
        let mut session = smelt_core::session::Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![
            user("old"),
            assistant("old reply"),
            user("recent"),
            assistant("recent reply"),
        ];
        session.checkpoint = Some(ContextCheckpoint {
            kind: "compaction".to_string(),
            summary: "summary text".to_string(),
            first_live_index: 2,
            created_at_ms: 0,
            tokens_before: None,
            tokens_after_estimate: None,
            ..Default::default()
        });
        let model = session.model_history("SUMMARY:");
        assert_eq!(model.len(), 3); // summary + recent + recent reply
        let first = &model[0];
        assert!(
            matches!(first, HistoryItem::User { content } if content.text_content().contains("summary text")),
            "first item should be the summary user message"
        );
    }

    #[test]
    fn checkpoint_cleared_on_rewind_past_it() {
        let mut session = smelt_core::session::Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![
            user("old"),
            assistant("old reply"),
            user("recent"),
            assistant("recent reply"),
        ];
        session.checkpoint = Some(ContextCheckpoint {
            kind: "compaction".to_string(),
            summary: "summary".to_string(),
            first_live_index: 2,
            created_at_ms: 0,
            tokens_before: None,
            tokens_after_estimate: None,
            ..Default::default()
        });
        // Simulate rewind to before the checkpoint
        session.history.truncate(1);
        if session
            .checkpoint
            .as_ref()
            .is_some_and(|cp| cp.first_live_index >= session.history.len())
        {
            session.checkpoint = None;
        }
        assert!(session.checkpoint.is_none());
    }

    #[test]
    fn checkpoint_survives_rewind_before_it() {
        let mut session = smelt_core::session::Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![
            user("old"),
            assistant("old reply"),
            user("recent"),
            assistant("recent reply"),
            user("newest"),
        ];
        session.checkpoint = Some(ContextCheckpoint {
            kind: "compaction".to_string(),
            summary: "summary".to_string(),
            first_live_index: 2,
            created_at_ms: 0,
            tokens_before: None,
            tokens_after_estimate: None,
            ..Default::default()
        });
        // Rewind to after checkpoint
        session.history.truncate(4);
        if session
            .checkpoint
            .as_ref()
            .is_some_and(|cp| cp.first_live_index >= session.history.len())
        {
            session.checkpoint = None;
        }
        assert!(session.checkpoint.is_some());
        assert_eq!(session.checkpoint.as_ref().unwrap().first_live_index, 2);
    }

    #[test]
    fn is_compaction_summary_detects_prefix() {
        let prefix = engine::SUMMARY_PREFIX;
        assert!(
            !prefix.is_empty(),
            "SUMMARY_PREFIX must be non-empty for this test"
        );
        let item = HistoryItem::user(Content::text(format!("{}\nhere is the summary", prefix)));
        assert!(is_compaction_summary_item(&item));

        let normal = HistoryItem::user(Content::text("hello world"));
        assert!(!is_compaction_summary_item(&normal));
    }

    #[test]
    fn truncate_keyed_pops_entries_beyond_idx() {
        let mut snaps: Vec<(usize, u32)> = vec![(1, 10), (3, 30), (5, 50)];
        truncate_keyed(&mut snaps, 4);
        assert_eq!(snaps, vec![(1, 10), (3, 30)]);
        truncate_keyed(&mut snaps, 0);
        assert!(snaps.is_empty());
    }

    #[test]
    fn install_context_checkpoint_clears_context_tokens() {
        // We can't call install_context_checkpoint without a full TuiApp,
        // but we can verify the Session state mutation directly.
        let mut session = smelt_core::session::Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![
            user("old"),
            assistant("old reply"),
            user("recent"),
            assistant("recent reply"),
        ];
        session.context_tokens = Some(500);
        session.checkpoint = Some(ContextCheckpoint {
            kind: "compaction".to_string(),
            summary: "summary".to_string(),
            first_live_index: 2,
            created_at_ms: 0,
            tokens_before: Some(500),
            tokens_after_estimate: None,
            ..Default::default()
        });
        // context_tokens must be cleared so the next turn's actual usage
        // becomes the authoritative count.
        session.context_tokens = None;
        assert!(session.context_tokens.is_none());
        assert_eq!(session.checkpoint.as_ref().unwrap().first_live_index, 2);
    }

    #[test]
    fn model_history_checkpoint_skip_count() {
        let mut session = smelt_core::session::Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![
            user("0"),
            assistant("0a"),
            user("1"),
            assistant("1a"),
            user("2"),
            assistant("2a"),
        ];
        session.checkpoint = Some(ContextCheckpoint {
            kind: "compaction".to_string(),
            summary: "s".to_string(),
            first_live_index: 4, // keep user("2") and assistant("2a")
            created_at_ms: 0,
            tokens_before: None,
            tokens_after_estimate: None,
            ..Default::default()
        });
        let model = session.model_history("PREFIX:");
        assert_eq!(model.len(), 3); // summary + user("2") + assistant("2a")
        assert!(
            matches!(&model[0], HistoryItem::User { content } if content.text_content().contains("s"))
        );
        assert_eq!(model[1..], vec![user("2"), assistant("2a")]);
    }

    #[test]
    fn set_history_logic_strips_summary_and_appends_tail() {
        // Simulate what set_history does when the engine returns a history
        // that includes the injected summary.
        let mut session = smelt_core::session::Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![
            user("old"),
            assistant("old reply"),
            user("recent"),
            assistant("recent reply"),
        ];
        session.checkpoint = Some(ContextCheckpoint {
            kind: "compaction".to_string(),
            summary: "the summary".to_string(),
            first_live_index: 2,
            created_at_ms: 0,
            tokens_before: None,
            tokens_after_estimate: None,
            ..Default::default()
        });

        // Engine returns model_history() = [summary, recent, recent_reply, new_assistant]
        let engine_response = vec![
            HistoryItem::user(Content::text(format!(
                "{}\nthe summary",
                engine::SUMMARY_PREFIX.trim_end()
            ))),
            user("recent"),
            assistant("recent reply"),
            assistant("new reply"),
        ];

        // Apply set_history logic
        let mut incoming = engine_response;
        if incoming.first().is_some_and(is_compaction_summary_item) {
            incoming.remove(0);
        }
        let cp = session.checkpoint.clone().unwrap();
        session.history.truncate(cp.first_live_index);
        session.history.extend(incoming);

        assert_eq!(session.history.len(), 5); // old + old_reply + recent + recent_reply + new_reply
        assert!(session.checkpoint.is_some());
        assert_eq!(
            session.history,
            vec![
                user("old"),
                assistant("old reply"),
                user("recent"),
                assistant("recent reply"),
                assistant("new reply"),
            ]
        );
    }

    #[test]
    fn set_history_logic_keeps_non_summary_first_item() {
        // If the engine somehow returns a history without the summary prefix,
        // we should not strip the first item.
        let mut session = smelt_core::session::Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![
            user("old"),
            assistant("old reply"),
            user("recent"),
            assistant("recent reply"),
        ];
        session.checkpoint = Some(ContextCheckpoint {
            kind: "compaction".to_string(),
            summary: "the summary".to_string(),
            first_live_index: 2,
            created_at_ms: 0,
            tokens_before: None,
            tokens_after_estimate: None,
            ..Default::default()
        });

        // Engine returns history without summary (unexpected but possible)
        let engine_response = vec![
            user("recent"),
            assistant("recent reply"),
            assistant("new reply"),
        ];

        let mut incoming = engine_response;
        if incoming.first().is_some_and(is_compaction_summary_item) {
            incoming.remove(0);
        }
        let cp = session.checkpoint.clone().unwrap();
        session.history.truncate(cp.first_live_index);
        session.history.extend(incoming);

        assert_eq!(session.history.len(), 5);
        assert_eq!(
            session.history,
            vec![
                user("old"),
                assistant("old reply"),
                user("recent"),
                assistant("recent reply"),
                assistant("new reply"),
            ]
        );
    }

    #[test]
    fn replace_history_clears_checkpoint() {
        let mut session = smelt_core::session::Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![user("a"), assistant("b")];
        session.checkpoint = Some(ContextCheckpoint {
            kind: "compaction".to_string(),
            summary: "s".to_string(),
            first_live_index: 1,
            created_at_ms: 0,
            tokens_before: None,
            tokens_after_estimate: None,
            ..Default::default()
        });
        session.cost_snapshots.push((2, 1.0));
        session.turn_metas.push((
            2,
            protocol::TurnMeta {
                elapsed_ms: 0,
                avg_tps: None,
                interrupted: false,
                tool_elapsed: std::collections::HashMap::new(),
            },
        ));
        session.context_tokens = Some(100);
        session.context_tokens_history_len = Some(2);
        session.visible_context_tokens = Some(100);

        // Simulate replace_history
        session.history = vec![user("x")];
        session.checkpoint = None;
        session.cost_snapshots.clear();
        session.turn_metas.clear();
        session.clear_context_tokens();

        assert!(session.checkpoint.is_none());
        assert!(session.cost_snapshots.is_empty());
        assert!(session.turn_metas.is_empty());
        assert!(session.context_tokens.is_none());
        assert!(session.context_tokens_history_len.is_none());
        assert!(session.visible_context_tokens.is_none());
    }

    #[test]
    fn rewind_keeps_baseline_and_cost() {
        let mut session = smelt_core::session::Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![user("a"), assistant("b"), user("c"), assistant("d")];
        session.cost_snapshots = vec![(2, 0.5), (4, 1.0)];
        session.session_cost_usd = 1.0;
        session.context_tokens = Some(100);
        session.context_tokens_history_len = Some(4);
        session.visible_context_tokens = Some(100);

        // Rewind to history index 2 (before user "c")
        let hist_idx = 2;
        session.history.truncate(hist_idx);
        truncate_keyed(&mut session.cost_snapshots, hist_idx);
        session.session_cost_usd = session
            .cost_snapshots
            .last()
            .map(|&(_, c)| c)
            .unwrap_or(0.0);

        assert_eq!(session.history.len(), 2);
        assert_eq!(session.context_tokens, Some(100));
        assert_eq!(session.context_tokens_history_len, Some(4));
        assert_eq!(session.visible_context_tokens, Some(100));
        assert_eq!(session.session_cost_usd, 0.5);
    }

    #[test]
    fn rewind_past_all_cost_snapshots_clears_cost_keeps_baseline() {
        let mut session = smelt_core::session::Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![user("a"), assistant("b")];
        session.cost_snapshots = vec![(2, 0.5)];
        session.session_cost_usd = 0.5;
        session.context_tokens = Some(50);
        session.context_tokens_history_len = Some(2);
        session.visible_context_tokens = Some(50);

        session.history.truncate(0);
        truncate_keyed(&mut session.cost_snapshots, 0);
        session.session_cost_usd = session
            .cost_snapshots
            .last()
            .map(|&(_, c)| c)
            .unwrap_or(0.0);

        assert_eq!(session.context_tokens, Some(50));
        assert_eq!(session.context_tokens_history_len, Some(2));
        assert_eq!(session.visible_context_tokens, Some(50));
        assert_eq!(session.session_cost_usd, 0.0);
    }

    #[test]
    fn rewind_past_checkpoint_restores_pre_checkpoint_baseline() {
        let mut session = smelt_core::session::Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![
            user("old"),
            assistant("old reply"),
            user("recent"),
            assistant("recent reply"),
        ];
        session.context_tokens = Some(100);
        session.context_tokens_history_len = Some(4);
        session.visible_context_tokens = Some(100);
        session.checkpoint = Some(ContextCheckpoint {
            kind: "compaction".to_string(),
            summary: "summary".to_string(),
            first_live_index: 2,
            created_at_ms: 0,
            tokens_before: Some(100),
            tokens_after_estimate: None,
            pre_checkpoint_context_tokens: Some(100),
            pre_checkpoint_context_history_len: Some(4),
        });

        // Rewind to before the checkpoint
        session.history.truncate(1);
        session.clear_checkpoint_if_rewound_to(session.history.len());

        assert!(session.checkpoint.is_none());
        assert_eq!(session.context_tokens, Some(100));
        assert_eq!(session.context_tokens_history_len, Some(4));
        assert_eq!(session.visible_context_tokens, Some(100));
    }

    #[test]
    fn rewind_keeps_checkpoint_keeps_post_checkpoint_baseline() {
        let mut session = smelt_core::session::Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![
            user("old"),
            assistant("old reply"),
            user("recent"),
            assistant("recent reply"),
            user("newest"),
        ];
        session.context_tokens = Some(80);
        session.context_tokens_history_len = Some(5);
        session.visible_context_tokens = Some(80);
        session.checkpoint = Some(ContextCheckpoint {
            kind: "compaction".to_string(),
            summary: "summary".to_string(),
            first_live_index: 2,
            created_at_ms: 0,
            tokens_before: Some(100),
            tokens_after_estimate: None,
            pre_checkpoint_context_tokens: Some(100),
            pre_checkpoint_context_history_len: Some(4),
        });

        // Rewind to after the checkpoint (hist len = 4)
        session.history.truncate(4);
        session.clear_checkpoint_if_rewound_to(session.history.len());

        assert!(session.checkpoint.is_some());
        assert_eq!(session.context_tokens, Some(80));
        assert_eq!(session.context_tokens_history_len, Some(5));
        assert_eq!(session.visible_context_tokens, Some(80));
    }
}
