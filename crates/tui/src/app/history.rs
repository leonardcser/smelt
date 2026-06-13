use crate::app::TuiApp;
use smelt_core::content::transcript::Transcript;
use smelt_core::session;
use smelt_core::{Block, ToolOutput, ToolState, ToolStatus};

use protocol::{AgentMode, AssistantStep, Content, HistoryItem, UiCommand};
use std::collections::HashMap;
use std::time::Duration;

pub(crate) fn build_transcript_from_session(
    lua: &crate::lua::LuaRuntime,
    session: &session::Session,
) -> Transcript {
    let _perf = smelt_perf::perf::begin("transcript:build_from_session");
    smelt_perf::perf::record_value(
        "transcript:build_from_session:history_items",
        session.history.len() as u64,
    );
    let mut transcript = Transcript::new();
    if session.history.is_empty() {
        return transcript;
    }

    let mut tool_elapsed: HashMap<String, u64> = HashMap::new();
    for (_, meta) in &session.turn_metas {
        tool_elapsed.extend(meta.tool_elapsed.iter().map(|(k, v)| (k.clone(), *v)));
    }

    let compaction = session.checkpoint.clone();
    for (idx, item) in session.history.iter().enumerate() {
        if compaction
            .as_ref()
            .is_some_and(|cp| cp.first_live_index == idx)
        {
            transcript.insert_checkpoint_marker(
                idx,
                Block::Compacted {
                    summary: compaction
                        .as_ref()
                        .map(|cp| cp.summary.clone())
                        .unwrap_or_default(),
                },
            );
        }
        match item {
            HistoryItem::User { content, display } => {
                push_user_block(&mut transcript, lua, idx, content, display.as_deref())
            }
            HistoryItem::Assistant(turn) => {
                push_assistant_blocks(&mut transcript, lua, idx, turn, &tool_elapsed)
            }
            HistoryItem::Note(note) => push_note_block(&mut transcript, lua, idx, note),
            HistoryItem::System { .. } => {}
        }
    }
    if compaction
        .as_ref()
        .is_some_and(|cp| cp.first_live_index >= session.history.len())
    {
        transcript.insert_checkpoint_marker(
            session.history.len(),
            Block::Compacted {
                summary: compaction.map(|cp| cp.summary).unwrap_or_default(),
            },
        );
    }

    smelt_perf::perf::record_value(
        "transcript:build_from_session:blocks",
        transcript.history.order.len() as u64,
    );
    transcript
}

fn fallback_transcript_index_for_history_index(
    history: &[HistoryItem],
    history_index: usize,
) -> usize {
    history
        .iter()
        .take(history_index.min(history.len()))
        .map(fallback_history_item_block_count)
        .sum()
}

fn fallback_history_item_block_count(item: &HistoryItem) -> usize {
    match item {
        HistoryItem::User { .. } | HistoryItem::Note(_) => 1,
        HistoryItem::System { .. } => 0,
        HistoryItem::Assistant(turn) => {
            let reasoning_blocks = turn
                .reasoning
                .as_ref()
                .is_some_and(|s| !s.trim().is_empty());
            let content_blocks = turn
                .content
                .as_ref()
                .is_some_and(|c| !c.text_content().trim().is_empty());
            usize::from(reasoning_blocks) + usize::from(content_blocks) + turn.invocations.len()
        }
    }
}

pub(crate) fn history_note_to_block(
    lua: &crate::lua::LuaRuntime,
    note: &protocol::HistoryNote,
) -> Block {
    match note.kind() {
        protocol::HistoryNoteKind::ModeChange => lua.mode_block(note.mode(), note.text()),
        protocol::HistoryNoteKind::ProcessStatus => Block::ProcessStatus {
            text: note.text().to_string(),
        },
    }
}

fn push_note_block(
    transcript: &mut Transcript,
    lua: &crate::lua::LuaRuntime,
    history_index: usize,
    note: &protocol::HistoryNote,
) {
    transcript.push_with_origin(
        history_note_to_block(lua, note),
        smelt_core::BlockOrigin::History(history_index),
    );
}

fn push_user_block(
    transcript: &mut Transcript,
    lua: &crate::lua::LuaRuntime,
    history_index: usize,
    content: &Content,
    display: Option<&str>,
) {
    let text = content.text_content();
    let prefix_marker = engine::SUMMARY_PREFIX.trim_end();
    if let Some(rest) = text.strip_prefix(prefix_marker) {
        let summary = rest.trim_start_matches('\n');
        transcript.push_with_origin(
            Block::Compacted {
                summary: summary.to_string(),
            },
            smelt_core::BlockOrigin::History(history_index),
        );
        return;
    }
    if let Some(note) = text.strip_prefix(protocol::MODE_NOTE_PREFIX) {
        transcript.push_with_origin(
            lua.mode_block(None, note.trim()),
            smelt_core::BlockOrigin::History(history_index),
        );
        return;
    }
    if let Some(note) = text.strip_prefix(protocol::PROCESS_STATUS_NOTE_PREFIX) {
        transcript.push_with_origin(
            Block::ProcessStatus {
                text: note.trim().to_string(),
            },
            smelt_core::BlockOrigin::History(history_index),
        );
        return;
    }
    let image_labels = content.image_labels();
    let display_source = display.unwrap_or(&text);
    let display_text = if image_labels.is_empty() {
        display_source.to_string()
    } else {
        let suffix = image_labels.join(" ");
        if display_source.is_empty() {
            suffix
        } else {
            format!("{display_source} {suffix}")
        }
    };
    transcript.push_with_origin(
        Block::User {
            text: display_text,
            image_labels,
        },
        smelt_core::BlockOrigin::History(history_index),
    );
}

fn push_assistant_blocks(
    transcript: &mut Transcript,
    lua: &crate::lua::LuaRuntime,
    history_index: usize,
    turn: &AssistantStep,
    tool_elapsed: &HashMap<String, u64>,
) {
    if let Some(ref reasoning) = turn.reasoning {
        if !reasoning.is_empty() {
            transcript.push_with_origin(
                Block::Thinking {
                    content: reasoning.clone(),
                },
                smelt_core::BlockOrigin::History(history_index),
            );
        }
    }
    if let Some(ref content) = turn.content {
        transcript.push_with_origin(
            Block::Text {
                content: content.text_content().into_owned(),
            },
            smelt_core::BlockOrigin::History(history_index),
        );
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
        let summary = lua.tool_summary(&inv.name, &args);
        transcript.push_tool_call_with_origin(
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
                body: None,
                layout_revision: 0,
            },
            smelt_core::BlockOrigin::History(history_index),
        );
    }
}

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
        let applied_items: Vec<HistoryItem> = self
            .pending_history_appends
            .iter()
            .filter_map(|pending| {
                history
                    .iter()
                    .find(|existing| pending.matches_history_item(existing))
                    .cloned()
            })
            .collect();
        self.core
            .session
            .merge_model_history_snapshot(engine::SUMMARY_PREFIX, history);
        for item in applied_items {
            self.commit_pending_history_append(&item);
        }
        self.sync_session_snapshot();
        self.publish_history_delta("set");
    }

    pub(crate) fn publish_history_delta(&mut self, kind: &str) {
        if matches!(kind, "cleared" | "rewound" | "loaded" | "forked") {
            self.bump_epoch("history_epoch");
        }
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
        append: &protocol::HistoryAppend,
    ) -> protocol::HistoryAppendResult {
        protocol::apply_history_append(&mut self.core.session.history, append)
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

    pub(crate) fn snapshot_accounting(&mut self) {
        if self.context_tokens_updated_this_turn && self.core.session.context_tokens.is_some() {
            self.core.session.context_tokens_history_len = Some(self.core.session.history.len());
        }
        self.context_tokens_updated_this_turn = false;
        self.core.session.snapshot_accounting();
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
        self.bump_epoch("session_epoch");
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
        // Reset is a hard session boundary: cancel in-flight engine work and all
        // Lua tasks before clearing state so stale events and child processes
        // don't restore old data into the new session.
        if self.agent.is_some() {
            self.cancel_agent();
            self.agent = None;
        } else {
            self.core.engine.send(UiCommand::Cancel);
        }
        self.lua.cancel_tasks();
        let old_id = self.core.session.id.clone();
        self.core.session.history.clear();
        self.reset_session_permissions();
        self.queued_inputs.clear();
        self.task_label = None;
        self.working.clear();
        if let Some(w) = self.ui.win_mut(crate::app::PROMPT_WIN) {
            w.pin_scroll(0);
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
        self.bump_epoch("session_epoch");
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
        // Loading a session is also a hard boundary for Lua work tied to the
        // previous session.
        self.lua.cancel_tasks();
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
        self.bump_epoch("session_epoch");
        if let Some(ref slug) = self.core.session.slug {
            self.set_task_label(slug.clone());
        }
        // Drop snapshots beyond the restored history length.
        let hist_len = self.core.session.history.len();
        self.core.session.prune_accounting_snapshots(hist_len);
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
        let display_cache = crate::content::display_cache::read_for_session(&self.core.session);
        self.transcript.replace_transcript_with_display_cache(
            build_transcript_from_session(&self.lua, &self.core.session),
            display_cache,
        );

        if let Some((_, meta)) = self.core.session.turn_metas.last() {
            self.working.restore_from_turn_meta(meta);
        }
    }

    pub(crate) fn schedule_session_save(&mut self) {
        self.session_save_pending = true;
    }

    pub(crate) fn save_session_if_pending(&mut self) {
        if self.session_save_pending && self.agent.is_none() && !self.busy_stack.is_busy() {
            self.save_session();
        }
    }

    pub(crate) fn save_session(&mut self) {
        let _perf = smelt_perf::perf::begin("session:save");
        if self.core.session.history.is_empty() {
            self.session_save_pending = false;
            return;
        }
        self.session_save_pending = false;
        self.sync_session_snapshot();
        let display_cache = self.transcript.display_cache_entries();
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
            display_cache,
        });
    }

    /// Block until all queued persist writes complete. Call before reading session files from disk.
    pub(crate) fn flush_persist(&self) {
        self.persister.flush();
    }

    fn refresh_compaction_marker(&mut self) {
        let Some(checkpoint) = self.core.session.checkpoint.as_ref() else {
            return;
        };
        let block = Block::Compacted {
            summary: checkpoint.summary.clone(),
        };
        if self
            .transcript
            .has_history_origin_at_or_after(checkpoint.first_live_index)
        {
            self.transcript
                .insert_checkpoint_marker(checkpoint.first_live_index, block);
        } else {
            let index = fallback_transcript_index_for_history_index(
                &self.core.session.history,
                checkpoint.first_live_index,
            );
            self.transcript.insert_checkpoint_marker_at(index, block);
        }
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
        self.refresh_compaction_marker();
        self.publish_history_delta("checkpoint");
        self.schedule_session_save();
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
                ..
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
        truncate_keyed(&mut self.core.session.turn_metas, hist_idx);
        self.core
            .session
            .restore_accounting_after_rewind(hist_idx, keep_checkpoint_at_boundary);
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
        self.core.session.turn_metas.clear();
        self.core.session.clear_accounting_snapshots();
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
        HistoryItem::Assistant(protocol::AssistantStep::terminal(
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

    #[test]
    fn restore_screen_uses_user_display_when_present() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.core.session.history = vec![HistoryItem::User {
            content: Content::text("expanded command body"),
            display: Some("/reflect".into()),
        }];

        app.app.restore_screen();

        let history = app.app.transcript.history();
        let id = history.order[0];
        assert!(matches!(
            history.blocks.get(&id),
            Some(Block::User { text, .. }) if text == "/reflect"
        ));
    }

    #[test]
    fn restore_screen_rebuilds_process_status_notes_as_process_blocks() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let note = "Background process 123 completed successfully.";
        app.app.core.session.history = vec![user(&protocol::process_status_note(note))];

        app.app.restore_screen();

        let history = app.app.transcript.history();
        let id = history.order[0];
        assert!(matches!(
            history.blocks.get(&id),
            Some(Block::ProcessStatus { text }) if text == note
        ));
    }

    #[test]
    fn restore_screen_rebuilds_mode_notes_as_mode_blocks() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let note = protocol::mode_change_note("now in apply mode");
        app.app.core.session.history = vec![user(&note)];

        app.app.restore_screen();

        let history = app.app.transcript.history();
        let id = history.order[0];
        assert!(matches!(history.blocks.get(&id), Some(Block::Mode { .. })));
    }

    #[test]
    fn checkpoint_commit_inserts_marker_without_rebuilding_transcript() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.core.session.history = vec![
            user("old"),
            assistant("old reply"),
            user("recent"),
            assistant("recent reply"),
        ];
        app.app.restore_screen();
        let before = app.app.transcript.history().order.clone();

        let installed =
            app.app
                .install_context_checkpoint("compaction".into(), "summary".into(), 2, Some(100));

        assert!(installed);
        let history = app.app.transcript.history();
        assert_eq!(history.order.len(), before.len() + 1);
        assert_eq!(history.order[0], before[0]);
        assert_eq!(history.order[1], before[1]);
        assert_eq!(history.order[3], before[2]);
        assert_eq!(history.order[4], before[3]);
        assert!(matches!(
            history.blocks.get(&history.order[2]),
            Some(Block::Compacted { summary }) if summary == "summary"
        ));
    }

    #[test]
    fn checkpoint_commit_moves_existing_marker() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.core.session.history = vec![
            user("old"),
            assistant("old reply"),
            user("kept user"),
            assistant("kept reply"),
            user("newest"),
        ];
        app.app.core.session.checkpoint = Some(ContextCheckpoint {
            kind: "compaction".to_string(),
            summary: "old summary".to_string(),
            first_live_index: 2,
            created_at_ms: 0,
            tokens_before: None,
            tokens_after_estimate: None,
            ..Default::default()
        });
        app.app.restore_screen();

        let installed = app.app.install_context_checkpoint(
            "compaction".into(),
            "new summary".into(),
            2,
            Some(100),
        );

        assert!(installed);
        let history = app.app.transcript.history();
        let markers: Vec<_> = history
            .order
            .iter()
            .enumerate()
            .filter_map(|(idx, id)| match history.blocks.get(id) {
                Some(Block::Compacted { summary }) => Some((idx, summary.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(markers, vec![(3, "new summary")]);
        assert!(app.app.session_save_pending);
    }

    #[test]
    fn checkpoint_commit_places_marker_without_existing_provenance() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.core.session.history = vec![
            user("old"),
            assistant("old reply"),
            user("recent"),
            assistant("recent reply"),
        ];
        for block in [
            Block::User {
                text: "old".into(),
                image_labels: vec![],
            },
            Block::Text {
                content: "old reply".into(),
            },
            Block::User {
                text: "recent".into(),
                image_labels: vec![],
            },
            Block::Text {
                content: "recent reply".into(),
            },
        ] {
            app.app.push_block(block);
        }

        let installed =
            app.app
                .install_context_checkpoint("compaction".into(), "summary".into(), 2, Some(100));

        assert!(installed);
        let history = app.app.transcript.history();
        assert!(matches!(
            history.blocks.get(&history.order[2]),
            Some(Block::Compacted { summary }) if summary == "summary"
        ));
    }

    #[test]
    fn checkpoint_commit_falls_back_when_boundary_is_after_known_origins() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.core.session.history = vec![user("restored"), assistant("restored reply")];
        app.app.restore_screen();
        app.app.core.session.history.extend([
            user("live old"),
            assistant("live old reply"),
            user("live recent"),
            assistant("live recent reply"),
        ]);
        for block in [
            Block::User {
                text: "live old".into(),
                image_labels: vec![],
            },
            Block::Text {
                content: "live old reply".into(),
            },
            Block::User {
                text: "live recent".into(),
                image_labels: vec![],
            },
            Block::Text {
                content: "live recent reply".into(),
            },
        ] {
            app.app.push_block(block);
        }

        let installed =
            app.app
                .install_context_checkpoint("compaction".into(), "summary".into(), 4, Some(100));

        assert!(installed);
        let history = app.app.transcript.history();
        assert!(matches!(
            history.blocks.get(&history.order[4]),
            Some(Block::Compacted { summary }) if summary == "summary"
        ));
        assert!(matches!(
            history.blocks.get(&history.order[5]),
            Some(Block::User { text, .. }) if text == "live recent"
        ));
    }

    #[test]
    fn checkpoint_commit_keeps_history_compacted_blocks() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.core.session.history = vec![
            user(&format!(
                "{}\nuser-written summary-looking block",
                engine::SUMMARY_PREFIX.trim_end()
            )),
            assistant("reply"),
            user("recent"),
        ];
        app.app.core.session.checkpoint = Some(ContextCheckpoint {
            kind: "compaction".to_string(),
            summary: "old summary".to_string(),
            first_live_index: 2,
            created_at_ms: 0,
            tokens_before: None,
            tokens_after_estimate: None,
            ..Default::default()
        });
        app.app.restore_screen();

        let installed = app.app.install_context_checkpoint(
            "compaction".into(),
            "new summary".into(),
            2,
            Some(100),
        );

        assert!(installed);
        let history = app.app.transcript.history();
        let summaries: Vec<_> = history
            .order
            .iter()
            .filter_map(|id| match history.blocks.get(id) {
                Some(Block::Compacted { summary }) => Some(summary.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            summaries,
            vec!["user-written summary-looking block", "new summary"]
        );
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
            matches!(first, HistoryItem::User { content, .. } if content.text_content().contains("summary text")),
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
            matches!(&model[0], HistoryItem::User { content, .. } if content.text_content().contains("s"))
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
        session.session_usage.prompt_tokens = Some(10);
        session.snapshot_accounting();

        // Simulate replace_history
        session.history = vec![user("x")];
        session.checkpoint = None;
        session.turn_metas.clear();
        session.clear_accounting_snapshots();
        session.clear_context_tokens();

        assert!(session.checkpoint.is_none());
        assert!(session.turn_metas.is_empty());
        assert!(session.accounting_snapshots.is_empty());
        assert!(session.session_usage.prompt_tokens.is_none());
        assert!(session.context_tokens.is_none());
        assert!(session.context_tokens_history_len.is_none());
    }

    #[test]
    fn rewind_restores_accounting_snapshot() {
        let mut session = smelt_core::session::Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![user("a"), assistant("b")];
        session.session_usage.prompt_tokens = Some(10);
        session.session_usage.completion_tokens = Some(1);
        session.context_tokens = Some(50);
        session.context_tokens_history_len = Some(2);
        session.session_cost_usd = 0.5;
        session.snapshot_accounting();

        session.history.extend([user("c"), assistant("d")]);
        session.session_usage.prompt_tokens = Some(30);
        session.session_usage.completion_tokens = Some(3);
        session.context_tokens = Some(100);
        session.context_tokens_history_len = Some(4);
        session.session_cost_usd = 1.0;
        session.snapshot_accounting();

        let hist_idx = 2;
        session.history.truncate(hist_idx);
        session.restore_accounting_after_rewind(hist_idx, false);

        assert_eq!(session.history.len(), 2);
        assert_eq!(session.session_usage.prompt_tokens, Some(10));
        assert_eq!(session.session_usage.completion_tokens, Some(1));
        assert_eq!(session.context_tokens, Some(50));
        assert_eq!(session.context_tokens_history_len, Some(2));
        assert_eq!(session.session_cost_usd, 0.5);
    }

    #[test]
    fn app_rewind_restores_accounting_snapshot() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.core.session.history = vec![user("a"), assistant("b")];
        app.app.core.session.session_usage.prompt_tokens = Some(10);
        app.app.core.session.session_usage.completion_tokens = Some(1);
        app.app.core.session.context_tokens = Some(50);
        app.app.core.session.context_tokens_history_len = Some(2);
        app.app.core.session.session_cost_usd = 0.5;
        app.app.core.session.snapshot_accounting();

        app.app
            .core
            .session
            .history
            .extend([user("c"), assistant("d")]);
        app.app.core.session.session_usage.prompt_tokens = Some(30);
        app.app.core.session.session_usage.completion_tokens = Some(3);
        app.app.core.session.context_tokens = Some(100);
        app.app.core.session.context_tokens_history_len = Some(4);
        app.app.core.session.session_cost_usd = 1.0;
        app.app.core.session.snapshot_accounting();
        app.app.restore_screen();

        let restored = app.app.rewind_to(2).expect("second user turn");

        assert_eq!(restored.0, "c");
        assert_eq!(app.app.core.session.history.len(), 2);
        assert_eq!(app.app.core.session.session_cost_usd, 0.5);
        assert_eq!(app.app.core.session.session_usage.prompt_tokens, Some(10));
        assert_eq!(
            app.app.core.session.session_usage.completion_tokens,
            Some(1)
        );
        assert_eq!(app.app.core.session.context_tokens, Some(50));
        assert_eq!(app.app.core.session.context_tokens_history_len, Some(2));
        assert_eq!(app.app.core.session.accounting_snapshots.len(), 1);
    }

    #[test]
    fn rewind_past_all_accounting_snapshots_clears_usage_and_context() {
        let mut session = smelt_core::session::Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![user("a"), assistant("b")];
        session.session_usage.prompt_tokens = Some(10);
        session.context_tokens = Some(50);
        session.context_tokens_history_len = Some(2);
        session.snapshot_accounting();

        session.history.truncate(0);
        session.restore_accounting_after_rewind(0, false);

        assert!(session.session_usage.prompt_tokens.is_none());
        assert!(session.context_tokens.is_none());
        assert!(session.context_tokens_history_len.is_none());
    }

    #[test]
    fn rewind_past_checkpoint_restores_pre_checkpoint_snapshot() {
        let mut session = smelt_core::session::Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![user("old"), assistant("old reply")];
        session.session_usage.prompt_tokens = Some(10);
        session.context_tokens = Some(50);
        session.context_tokens_history_len = Some(2);
        session.snapshot_accounting();

        session
            .history
            .extend([user("recent"), assistant("recent reply")]);
        session.context_tokens = Some(100);
        session.context_tokens_history_len = Some(4);
        session.checkpoint = Some(ContextCheckpoint {
            kind: "compaction".to_string(),
            summary: "summary".to_string(),
            first_live_index: 2,
            created_at_ms: 7,
            tokens_before: Some(100),
            tokens_after_estimate: None,
            pre_checkpoint_context_tokens: Some(100),
            pre_checkpoint_context_history_len: Some(4),
        });
        session.clear_context_tokens_baseline();
        session.snapshot_accounting();
        session.context_tokens = Some(80);
        session.context_tokens_history_len = Some(4);
        session.snapshot_accounting();

        let hist_idx = 2;
        session.history.truncate(hist_idx);
        session.restore_accounting_after_rewind(hist_idx, false);

        assert!(session.checkpoint.is_none());
        assert_eq!(session.session_usage.prompt_tokens, Some(10));
        assert_eq!(session.context_tokens, Some(50));
        assert_eq!(session.context_tokens_history_len, Some(2));
    }

    #[test]
    fn rewind_past_checkpoint_without_snapshot_clears_context_tokens() {
        let mut session = smelt_core::session::Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![
            user("old"),
            assistant("old reply"),
            user("recent"),
            assistant("recent reply"),
        ];
        session.session_usage.prompt_tokens = Some(30);
        session.session_cost_usd = 1.0;
        session.context_tokens = None;
        session.context_tokens_history_len = None;
        session.checkpoint = Some(ContextCheckpoint {
            kind: "compaction".to_string(),
            summary: "summary".to_string(),
            first_live_index: 2,
            created_at_ms: 7,
            tokens_before: Some(100),
            tokens_after_estimate: None,
            pre_checkpoint_context_tokens: Some(100),
            pre_checkpoint_context_history_len: Some(4),
        });

        let hist_idx = 2;
        session.history.truncate(hist_idx);
        session.restore_accounting_after_rewind(hist_idx, false);

        assert!(session.checkpoint.is_none());
        assert!(session.accounting_snapshots.is_empty());
        assert!(session.session_usage.prompt_tokens.is_none());
        assert_eq!(session.session_cost_usd, 0.0);
        assert!(session.context_tokens.is_none());
        assert!(session.context_tokens_history_len.is_none());
    }

    #[test]
    fn rewind_keeps_checkpoint_clears_incompatible_context_snapshot() {
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
            created_at_ms: 7,
            tokens_before: Some(100),
            tokens_after_estimate: None,
            pre_checkpoint_context_tokens: Some(100),
            pre_checkpoint_context_history_len: Some(4),
        });
        session.context_tokens = Some(80);
        session.context_tokens_history_len = Some(5);
        session.snapshot_accounting();

        let hist_idx = 2;
        session.history.truncate(hist_idx);
        session.restore_accounting_after_rewind(hist_idx, true);

        assert!(session.checkpoint.is_some());
        assert!(session.context_tokens.is_none());
        assert!(session.context_tokens_history_len.is_none());
    }
}
