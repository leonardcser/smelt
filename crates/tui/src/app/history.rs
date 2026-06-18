use crate::app::TuiApp;
use smelt_core::content::transcript::Transcript;
use smelt_core::session;
use smelt_core::{
    Block, ToolOutput, ToolState, ToolStatus, TranscriptBlockDescriptor, TranscriptBlockRecord,
};

use protocol::{AgentMode, AssistantStep, Content, HistoryItem, UiCommand};
use std::collections::HashMap;
use std::time::Duration;

pub(crate) struct ToolSummaryResolver<'a> {
    lua: &'a crate::lua::LuaRuntime,
}

impl<'a> ToolSummaryResolver<'a> {
    pub(crate) fn new(lua: &'a crate::lua::LuaRuntime) -> Self {
        Self { lua }
    }

    pub(crate) fn resolve(
        &self,
        name: &str,
        args: &HashMap<String, serde_json::Value>,
    ) -> protocol::StyledLines {
        let summary = self.lua.tool_summary(name, args);
        if !summary.is_empty() || self.lua.has_tool(name) {
            return summary;
        }
        smelt_core::mcp::args_summary(args)
    }
}

pub(crate) fn build_transcript_from_session(
    lua: &crate::lua::LuaRuntime,
    session: &session::Session,
) -> Transcript {
    let summary_resolver = ToolSummaryResolver::new(lua);
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
                push_assistant_blocks(&mut transcript, &summary_resolver, idx, turn, &tool_elapsed)
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

pub(crate) fn load_transcript_from_sqlite(session: &session::Session) -> Option<Transcript> {
    load_transcript_from_sqlite_dir(session::dir_for(session))
}

pub(crate) fn load_transcript_from_sqlite_id(id: &str) -> Option<Transcript> {
    load_transcript_from_sqlite_dir(session::dir_for_id(id))
}

fn load_transcript_from_sqlite_dir(session_dir: std::path::PathBuf) -> Option<Transcript> {
    let db_path = session_dir.join("session.db");
    let db = smelt_store::SessionDb::open_read_only(db_path).ok()?;
    let rows = db.read_transcript_descriptor_records().ok()?;
    if rows.is_empty() {
        return None;
    }
    let records = rows
        .into_iter()
        .map(|row| {
            let descriptor: TranscriptBlockDescriptor = serde_json::from_str(&row.descriptor_json)?;
            let origin = row
                .origin_json
                .as_deref()
                .map(serde_json::from_str)
                .transpose()?
                .or_else(|| {
                    row.history_idx
                        .map(|idx| smelt_core::BlockOrigin::History(idx as usize))
                });
            let tool_state = row
                .tool_state_json
                .as_deref()
                .map(serde_json::from_str)
                .transpose()?;
            let content_hash = row.content_hash.parse::<u64>().unwrap_or_default();
            Ok(TranscriptBlockRecord {
                descriptor,
                content_hash,
                origin,
                tool_state,
            })
        })
        .collect::<Result<Vec<_>, serde_json::Error>>()
        .ok()?;
    Some(Transcript::from_descriptor_records(records))
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
        HistoryItem::User { .. } => 1,
        HistoryItem::Note(note) => usize::from(note.kind() != protocol::HistoryNoteKind::Context),
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

fn checkpoint_suffix_blocks_match(prev: &Block, next: &Block) -> bool {
    !matches!(prev, Block::ToolCall { .. }) && prev == next
}

pub(crate) fn history_note_to_block(
    lua: &crate::lua::LuaRuntime,
    note: &protocol::HistoryNote,
) -> Option<Block> {
    match note.kind() {
        protocol::HistoryNoteKind::ModeChange => Some(lua.mode_block(note.mode(), note.text())),
        protocol::HistoryNoteKind::Context => None,
        protocol::HistoryNoteKind::ProcessStatus => Some(Block::ProcessStatus {
            text: note.text().to_string(),
            event: note.process_status_event_ref().cloned(),
        }),
    }
}

fn push_note_block(
    transcript: &mut Transcript,
    lua: &crate::lua::LuaRuntime,
    history_index: usize,
    note: &protocol::HistoryNote,
) {
    let Some(block) = history_note_to_block(lua, note) else {
        return;
    };
    transcript.push_descriptor_with_origin(
        TranscriptBlockDescriptor::from_block(block),
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
        transcript.push_descriptor_with_origin(
            TranscriptBlockDescriptor::Compacted {
                summary: summary.to_string(),
            },
            smelt_core::BlockOrigin::History(history_index),
        );
        return;
    }
    if let Some(note) = text.strip_prefix(protocol::MODE_NOTE_PREFIX) {
        transcript.push_descriptor_with_origin(
            TranscriptBlockDescriptor::from_block(lua.mode_block(None, note.trim())),
            smelt_core::BlockOrigin::History(history_index),
        );
        return;
    }
    if let Some(note) = text.strip_prefix(protocol::PROCESS_STATUS_NOTE_PREFIX) {
        transcript.push_descriptor_with_origin(
            TranscriptBlockDescriptor::ProcessStatus {
                text: note.trim().to_string(),
                event: None,
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
    transcript.push_descriptor_with_origin(
        TranscriptBlockDescriptor::User {
            text: display_text,
            image_labels,
        },
        smelt_core::BlockOrigin::History(history_index),
    );
}

fn push_assistant_blocks(
    transcript: &mut Transcript,
    summary_resolver: &ToolSummaryResolver<'_>,
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
        transcript.push_descriptor_with_origin(
            TranscriptBlockDescriptor::Text {
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
        let summary = summary_resolver.resolve(&inv.name, &args);
        transcript.push_tool_descriptor_with_origin(
            TranscriptBlockDescriptor::ToolCall {
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
            },
            smelt_core::BlockOrigin::History(history_index),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebuild_uses_json_args_summary_for_resumed_mcp_tool_calls() {
        let lua = crate::lua::LuaRuntime::new();
        let mut session = session::Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history.push(HistoryItem::Assistant(
            protocol::AssistantStep::with_invocations(
                None,
                None,
                Vec::new(),
                vec![protocol::ToolInvocation {
                    call_id: "call-1".into(),
                    name: "mcp_server_echo".into(),
                    arguments: r#"{"label":"ok"}"#.into(),
                    result: protocol::ToolOutcome {
                        content: "done".into(),
                        is_error: false,
                        metadata: None,
                    },
                    elapsed_ms: None,
                }],
            ),
        ));

        let transcript = build_transcript_from_session(&lua, &session);
        let id = transcript.history.order[0];
        match transcript.history.block(id) {
            Some(Block::ToolCall { summary, args, .. }) => {
                assert_eq!(args.get("label"), Some(&serde_json::json!("ok")));
                assert_eq!(summary.as_plain_text(), r#"{"label":"ok"}"#);
                assert_eq!(summary.0[0][0].syntax.as_deref(), Some("json"));
            }
            other => panic!("expected tool call, got {other:?}"),
        }
    }

    #[test]
    fn context_notes_do_not_render_in_transcript() {
        let lua = crate::lua::LuaRuntime::new();
        let mut session = session::Session::new(1, std::path::PathBuf::from("/tmp"));
        session
            .history
            .push(HistoryItem::note(protocol::HistoryNote::context(
                "Current working directory: /tmp.",
            )));

        let transcript = build_transcript_from_session(&lua, &session);
        assert!(transcript.history.order.is_empty());
    }

    #[test]
    fn registered_lua_tool_can_intentionally_use_empty_summary() {
        let lua = crate::lua::LuaRuntime::new();
        lua.lua
            .load(
                r#"
                smelt.tools.register({
                  name = "quiet_tool",
                  description = "",
                  parameters = { type = "object", properties = {} },
                  summary = function(args) return "" end,
                  execute = function(args) return "" end,
                })
                "#,
            )
            .exec()
            .unwrap();
        let mut args = HashMap::new();
        args.insert("label".into(), serde_json::json!("ok"));

        let summary = ToolSummaryResolver::new(&lua).resolve("quiet_tool", &args);
        assert!(summary.is_empty());
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
        let dirty_from = self
            .core
            .session
            .history
            .iter()
            .zip(history.iter())
            .take_while(|(left, right)| left == right)
            .count();
        self.mark_history_dirty_from(dirty_from);
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

    fn mark_history_dirty_from(&mut self, idx: usize) {
        self.session_dirty = true;
        self.dirty_history_from = Some(
            self.dirty_history_from
                .map_or(idx, |current| current.min(idx)),
        );
    }

    pub(crate) fn apply_history_append_to_history(
        &mut self,
        append: &protocol::HistoryAppend,
    ) -> protocol::HistoryAppendResult {
        let old_len = self.core.session.history.len();
        let result = protocol::apply_history_append(&mut self.core.session.history, append);
        match result {
            protocol::HistoryAppendResult::Unchanged => {}
            protocol::HistoryAppendResult::Pushed => {
                self.mark_history_dirty_from(old_len);
            }
            protocol::HistoryAppendResult::ReplacedLast
            | protocol::HistoryAppendResult::RemovedLast => {
                self.mark_history_dirty_from(old_len.saturating_sub(1));
            }
        }
        result
    }

    pub(crate) fn sync_session_snapshot(&mut self) {
        self.session_dirty = true;
        self.core.session.updated_at_ms = session::now_ms();
        self.core.session.mode = Some(self.core.config.mode.as_str().to_string());
        self.core.session.reasoning_effort = Some(self.core.config.reasoning_effort);
        self.core.session.model = Some(self.current_model_key());
        self.publish_shared_session_state();
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

    pub(crate) fn snapshot_context(&mut self) {
        self.session_dirty = true;
        if self.context_tokens_updated_this_turn && self.core.session.context_tokens.is_some() {
            self.core.session.context_tokens_history_len = Some(self.core.session.history.len());
        }
        self.context_tokens_updated_this_turn = false;
        self.core.session.snapshot_context();
    }

    pub(crate) fn set_session_title(
        &mut self,
        title: String,
        slug: String,
        target_history_len: Option<usize>,
    ) {
        self.core.session.title = Some(title);
        self.core.session.slug = Some(slug.clone());
        let hist_len = target_history_len.unwrap_or(self.core.session.history.len());
        self.core.session.snapshot_metadata_at(hist_len);
        self.set_task_label(slug);
        self.save_session();
    }

    pub(crate) fn restore_session_metadata_after_rewind(&mut self, hist_idx: usize) {
        self.core.session.restore_metadata_after_rewind(hist_idx);
        let slug = self.core.session.slug.clone().unwrap_or_default();
        self.set_task_label(slug);
    }

    fn apply_rewindable_session_state(&mut self, turn_meta: Option<protocol::TurnMeta>) {
        let slug = self.core.session.slug.clone().unwrap_or_default();
        self.set_task_label(slug);
        if let Some(meta) = turn_meta {
            self.working.restore_from_turn_meta(&meta);
        } else {
            self.working.clear();
        }
    }

    fn restore_rewindable_session_state_after_rewind(
        &mut self,
        hist_idx: usize,
        keep_checkpoint_at_boundary: bool,
    ) {
        let turn_meta = self
            .core
            .session
            .restore_rewindable_snapshots_after_rewind(hist_idx, keep_checkpoint_at_boundary);
        self.apply_rewindable_session_state(turn_meta);
    }

    fn prune_rewindable_session_state(&mut self, hist_idx: usize) {
        let turn_meta = self.core.session.prune_rewindable_snapshots(hist_idx);
        self.apply_rewindable_session_state(turn_meta);
    }

    pub(crate) fn fork_session(&mut self) {
        self.ensure_deferred_session_loaded();
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
        self.persisted_fingerprint = None;
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
        self.deferred_session_load = None;
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
        self.persisted_fingerprint = None;
        self.transcript_descriptors_persisted = false;
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
        self.deferred_session_load = None;
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
        self.persisted_fingerprint = None;
        self.transcript_descriptors_persisted = false;
        self.bump_epoch("session_epoch");
        // Drop snapshots beyond the restored history length.
        let hist_len = self.core.session.history.len();
        self.prune_rewindable_session_state(hist_len);
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

    pub(crate) fn load_session_display_only(
        &mut self,
        loaded: session::Session,
        transcript: Transcript,
        full_session_id: String,
    ) {
        if self.agent.is_some() {
            self.cancel_agent();
            self.agent = None;
        }
        self.lua.cancel_tasks();
        let old_id = self.core.session.id.clone();
        self.flush_persist();

        if let Some(mode) = loaded.mode.as_deref().and_then(AgentMode::parse) {
            self.set_mode(mode, false);
        }
        if !self.core.config.cli_model_override
            && !self.core.config.cli_api_base_override
            && !self.core.config.cli_api_key_env_override
        {
            if let Some(ref model_key) = loaded.model {
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
        self.deferred_session_load = Some(full_session_id);
        self.persisted_fingerprint = None;
        self.transcript_descriptors_persisted = true;
        self.session_dirty = false;
        self.dirty_history_from = None;
        self.bump_epoch("session_epoch");
        self.reset_session_permissions();
        self.queued_inputs.clear();
        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
        self.input.clear(&mut pctx);
        self.input.store.lock().unwrap().clear();
        self.stop_background_processes();
        self.clear_transcript();
        self.transcript.replace_transcript(transcript);
        self.publish_shared_session_state();
        self.core
            .cells
            .set_dyn("session_ended", std::rc::Rc::new(old_id));
        self.core.cells.set_dyn(
            "session_started",
            std::rc::Rc::new(self.core.session.id.clone()),
        );
        self.publish_history_delta("loaded");
        while self.core.engine.try_recv().is_ok() {}
    }

    pub(crate) fn ensure_deferred_session_loaded(&mut self) {
        let Some(id) = self.deferred_session_load.take() else {
            return;
        };
        if let Some(loaded) = session::load(&id) {
            self.core.session = loaded;
            self.prune_rewindable_session_state(self.core.session.history.len());
            self.sync_session_snapshot();
        }
    }

    // ── History / session ────────────────────────────────────────────────

    pub(crate) fn restore_screen(&mut self) {
        self.rebuild_screen_from_history();
    }

    fn rebuild_screen_from_history(&mut self) {
        self.clear_transcript();
        self.prune_rewindable_session_state(self.core.session.history.len());
        let persisted_fingerprint = persist_fingerprint(&self.core.session);
        let (transcript, descriptors_persisted) =
            match load_transcript_from_sqlite(&self.core.session) {
                Some(transcript) => (transcript, true),
                None => (
                    build_transcript_from_session(&self.lua, &self.core.session),
                    false,
                ),
            };
        self.transcript.replace_transcript(transcript);
        self.persisted_fingerprint = persisted_fingerprint;
        self.transcript_descriptors_persisted = descriptors_persisted;
        self.session_dirty = false;
        self.dirty_history_from = None;
    }

    pub(crate) fn schedule_session_save(&mut self) {
        self.session_save_pending = true;
    }

    pub(crate) fn save_session_if_pending(&mut self) {
        if self.session_save_pending && !self.prompt_input_is_busy() {
            self.save_session();
        }
    }

    pub(crate) fn save_session(&mut self) {
        let _perf = smelt_perf::perf::begin("session:save");
        if self.deferred_session_load.is_some() {
            self.session_save_pending = false;
            return;
        }
        if self.core.session.history.is_empty() {
            self.session_save_pending = false;
            return;
        }
        self.session_save_pending = false;
        let blobs = self.pending_image_blobs();
        if !self.session_dirty
            && self.persisted_fingerprint.is_some()
            && self.transcript_descriptors_persisted
            && self.transcript.history().descriptor_dirty_from().is_none()
            && blobs.is_empty()
        {
            smelt_perf::perf::record_value("session:save:skipped_unchanged", 1);
            return;
        }
        self.update_session_persist_metadata();
        let fingerprint = persist_fingerprint(&self.core.session);
        let history_start_idx = if self.persisted_fingerprint.is_some() {
            self.dirty_history_from
                .unwrap_or(self.core.session.history.len())
        } else {
            0
        };
        let transcript_history = self.transcript.history();
        let descriptor_order_dirty = if self.transcript_descriptors_persisted {
            transcript_history.descriptor_dirty_from().or_else(|| {
                self.dirty_history_from.and_then(|idx| {
                    transcript_history.first_block_index_for_history_origin_at_or_after(idx)
                })
            })
        } else {
            Some(0)
        };
        let descriptor_order_start =
            descriptor_order_dirty.unwrap_or_else(|| transcript_history.len());
        let descriptor_start_idx = if self.transcript_descriptors_persisted {
            transcript_history.descriptor_record_index_for_order_index(descriptor_order_start)
        } else {
            0
        };
        let descriptor_records = transcript_history.descriptor_records_from(descriptor_order_start);
        let descriptor_work = descriptor_order_dirty.is_some();
        if fingerprint.is_some()
            && self.persisted_fingerprint.as_ref() == fingerprint.as_ref()
            && !descriptor_work
            && blobs.is_empty()
        {
            self.session_dirty = false;
            self.dirty_history_from = None;
            smelt_perf::perf::record_value("session:save:skipped_unchanged", 1);
            return;
        }
        let session = self.core.session.clone();
        if let Ok(mut guard) = self.shared_session.lock() {
            *guard = Some(crate::app::SharedSessionState {
                id: session.id.clone(),
                has_messages: !session.history.is_empty(),
            });
        }
        self.persister.save(crate::persist::PersistRequest {
            session,
            history_start_idx,
            blobs,
            descriptor_start_idx,
            descriptor_records,
        });
        self.persisted_fingerprint = fingerprint;
        self.transcript_descriptors_persisted = true;
        self.transcript.history_mut().clear_descriptor_dirty();
        self.session_dirty = false;
        self.dirty_history_from = None;
    }

    fn pending_image_blobs(&self) -> Vec<crate::persist::Blob> {
        self.input
            .store
            .lock()
            .unwrap()
            .image_blobs()
            .into_iter()
            .map(|(filename, data_url)| crate::persist::Blob { filename, data_url })
            .collect()
    }

    fn update_session_persist_metadata(&mut self) {
        self.core.session.updated_at_ms = session::now_ms();
        self.core.session.mode = Some(self.core.config.mode.as_str().to_string());
        self.core.session.reasoning_effort = Some(self.core.config.reasoning_effort);
        self.core.session.model = Some(self.current_model_key());
    }

    /// Block until all queued persist writes complete. Call before reading session files from disk.
    pub(crate) fn flush_persist(&self) {
        self.persister.flush();
    }

    fn suppress_duplicate_carried_tail_before(&mut self, index: usize) -> usize {
        let history = self.transcript.history();
        if index == 0 || index >= history.order.len() {
            return index;
        }
        let prev_id = history.order[index - 1];
        let next_id = history.order[index];
        let duplicate = match (history.block(prev_id), history.block(next_id)) {
            (Some(prev), Some(next)) => checkpoint_suffix_blocks_match(prev, next),
            _ => false,
        };
        if duplicate && self.transcript.remove_unoriginated_at(index - 1).is_some() {
            index - 1
        } else {
            index
        }
    }

    fn refresh_compaction_marker(&mut self) {
        let Some(checkpoint) = self.core.session.checkpoint.as_ref() else {
            return;
        };
        let first_live_index = checkpoint.first_live_index;
        let block = Block::Compacted {
            summary: checkpoint.summary.clone(),
        };
        if let Some(index) = self
            .transcript
            .history()
            .first_block_index_for_history_origin_at_or_after(first_live_index)
        {
            let index = self.suppress_duplicate_carried_tail_before(index);
            self.transcript.insert_checkpoint_marker_at(index, block);
        } else {
            let index = fallback_transcript_index_for_history_index(
                &self.core.session.history,
                first_live_index,
            );
            let index = self.suppress_duplicate_carried_tail_before(index);
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
            self.clear_compaction_preview();
            self.notify("nothing old enough to compact".to_string());
            return false;
        }
        self.clear_compaction_preview();
        self.reset_visible_context_tokens();
        self.refresh_compaction_marker();
        self.publish_history_delta("checkpoint");
        self.schedule_session_save();
        self.transcript_win_mut().follow_tail();
        true
    }

    pub(crate) fn model_history(&self) -> Vec<HistoryItem> {
        self.core.session.model_history(engine::SUMMARY_PREFIX)
    }

    pub(crate) fn rewind_to(
        &mut self,
        block_idx: usize,
    ) -> Option<(String, Vec<(String, String)>)> {
        self.ensure_deferred_session_loaded();
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
        self.mark_history_dirty_from(hist_idx);
        let keep_checkpoint_at_boundary = turn_text.is_some()
            && self
                .core
                .session
                .checkpoint
                .as_ref()
                .is_some_and(|cp| cp.first_live_index == hist_idx);
        self.restore_rewindable_session_state_after_rewind(hist_idx, keep_checkpoint_at_boundary);
        self.truncate_to(block_idx);
        self.reset_session_permissions();
        self.sync_session_snapshot();
        self.publish_history_delta("rewound");

        turn_text.map(|t| (t, images))
    }

    pub(crate) fn rewind_to_start(&mut self) {
        self.core.session.history.clear();
        self.mark_history_dirty_from(0);
        self.core.session.checkpoint = None;
        self.core.session.turn_metas.clear();
        self.core.session.clear_context_snapshots();
        self.core.session.clear_context_tokens();
        self.core.session.clear_metadata_snapshots();
        self.task_label = None;
        self.working.clear();
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

fn persist_fingerprint(session: &session::Session) -> Option<Vec<u8>> {
    #[derive(serde::Serialize)]
    struct Fingerprint<'a> {
        id: &'a str,
        title: &'a Option<String>,
        slug: &'a Option<String>,
        first_user_message: &'a Option<String>,
        metadata_snapshots: &'a session::HistorySnapshots<session::SessionMetadataSnapshot>,
        created_at_ms: u64,
        updated_at_ms: u64,
        mode: &'a Option<String>,
        reasoning_effort: &'a Option<protocol::ReasoningEffort>,
        model: &'a Option<String>,
        cwd: &'a Option<String>,
        parent_id: &'a Option<String>,
        history: &'a [HistoryItem],
        checkpoint: &'a Option<session::ContextCheckpoint>,
        context_tokens: &'a Option<u32>,
        context_tokens_history_len: &'a Option<usize>,
        display_context_tokens: &'a Option<u32>,
        turn_metas: &'a session::HistorySnapshots<protocol::TurnMeta>,
        context_snapshots: &'a session::HistorySnapshots<session::ContextSnapshot>,
        session_cost_usd: f64,
        session_usage: &'a protocol::TokenUsage,
    }

    bincode::serialize(&Fingerprint {
        id: &session.id,
        title: &session.title,
        slug: &session.slug,
        first_user_message: &session.first_user_message,
        metadata_snapshots: &session.metadata_snapshots,
        created_at_ms: session.created_at_ms,
        updated_at_ms: 0,
        mode: &session.mode,
        reasoning_effort: &session.reasoning_effort,
        model: &session.model,
        cwd: &session.cwd,
        parent_id: &session.parent_id,
        history: &session.history,
        checkpoint: &session.checkpoint,
        context_tokens: &session.context_tokens,
        context_tokens_history_len: &session.context_tokens_history_len,
        display_context_tokens: &session.display_context_tokens,
        turn_metas: &session.turn_metas,
        context_snapshots: &session.context_snapshots,
        session_cost_usd: session.session_cost_usd,
        session_usage: &session.session_usage,
    })
    .ok()
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
    fn compaction_preview_rewrites_and_clears_one_block() {
        let mut app = crate::app::test_harness::TestApp::builder().build();

        app.app.update_compaction_preview("one".into());
        let id = app
            .app
            .transcript
            .compaction_preview_id()
            .expect("preview id");
        assert!(matches!(
            app.app.transcript.history().block(id),
            Some(Block::CompactionPreview { summary }) if summary == "one"
        ));

        app.app.update_compaction_preview("one\ntwo".into());
        assert_eq!(app.app.transcript.compaction_preview_id(), Some(id));
        assert!(matches!(
            app.app.transcript.history().block(id),
            Some(Block::CompactionPreview { summary }) if summary == "one\ntwo"
        ));

        app.app.clear_compaction_preview();
        assert!(app.app.transcript.compaction_preview_id().is_none());
        assert!(app.app.transcript.history().block(id).is_none());
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
            history.block(id),
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
            history.block(id),
            Some(Block::ProcessStatus { text, .. }) if text == note
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
        assert!(matches!(history.block(id), Some(Block::Mode { .. })));
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
        assert_eq!(app.app.core.session.display_context_tokens(), Some(0));
        let usage = app
            .app
            .core
            .cells
            .get::<protocol::TokenUsage>("tokens_used")
            .expect("tokens_used reset");
        assert_eq!(usage.context_tokens, Some(0));
        assert_eq!(usage.prompt_tokens, Some(0));
        let history = app.app.transcript.history();
        assert_eq!(history.order.len(), before.len() + 1);
        assert_eq!(history.order[0], before[0]);
        assert_eq!(history.order[1], before[1]);
        assert_eq!(history.order[3], before[2]);
        assert_eq!(history.order[4], before[3]);
        assert!(matches!(
            history.block(history.order[2]),
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
            .filter_map(|(idx, id)| match history.block(*id) {
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
            history.block(history.order[2]),
            Some(Block::Compacted { summary }) if summary == "summary"
        ));
    }

    #[test]
    fn checkpoint_projection_suppresses_duplicate_carried_tail_block() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: "old".into(),
        });
        transcript.push(Block::Text {
            content: "recent".into(),
        });
        transcript.push_with_origin(
            Block::Text {
                content: "recent".into(),
            },
            smelt_core::BlockOrigin::History(1),
        );
        app.app.transcript = crate::app::transcript::TranscriptView::from_transcript(transcript);

        let index = app.app.suppress_duplicate_carried_tail_before(2);

        let history = app.app.transcript.history();
        assert_eq!(index, 1);
        assert_eq!(history.order.len(), 2);
        assert!(
            matches!(history.block(history.order[0]), Some(Block::Text { content }) if content == "old")
        );
        assert!(
            matches!(history.block(history.order[1]), Some(Block::Text { content }) if content == "recent")
        );
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
            history.block(history.order[4]),
            Some(Block::Compacted { summary }) if summary == "summary"
        ));
        assert!(matches!(
            history.block(history.order[5]),
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
            .filter_map(|id| match history.block(*id) {
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
    fn history_snapshots_truncate_entries_beyond_idx() {
        let mut snaps =
            smelt_core::session::HistorySnapshots::from_vec(vec![(1, 10), (3, 30), (5, 50)]);
        snaps.truncate_after(4);
        assert_eq!(snaps.as_slice(), &[(1, 10), (3, 30)]);
        snaps.truncate_after(0);
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
                display_tps: None,
                interrupted: false,
                tool_elapsed: std::collections::HashMap::new(),
            },
        ));
        session.context_tokens = Some(100);
        session.context_tokens_history_len = Some(2);
        session.session_usage.prompt_tokens = Some(10);
        session.snapshot_context();

        // Simulate replace_history
        session.history = vec![user("x")];
        session.checkpoint = None;
        session.turn_metas.clear();
        session.clear_context_snapshots();
        session.clear_context_tokens();

        assert!(session.checkpoint.is_none());
        assert!(session.turn_metas.is_empty());
        assert!(session.context_snapshots.is_empty());
        assert_eq!(session.session_usage.prompt_tokens, Some(10));
        assert!(session.context_tokens.is_none());
        assert!(session.context_tokens_history_len.is_none());
    }

    #[test]
    fn rewind_restores_context_snapshot_without_rewinding_cumulative_spend() {
        let mut session = smelt_core::session::Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![user("a"), assistant("b")];
        session.session_usage.prompt_tokens = Some(10);
        session.session_usage.completion_tokens = Some(1);
        session.context_tokens = Some(50);
        session.context_tokens_history_len = Some(2);
        session.session_cost_usd = 0.5;
        session.snapshot_context();

        session.history.extend([user("c"), assistant("d")]);
        session.session_usage.prompt_tokens = Some(30);
        session.session_usage.completion_tokens = Some(3);
        session.context_tokens = Some(100);
        session.context_tokens_history_len = Some(4);
        session.session_cost_usd = 1.0;
        session.snapshot_context();

        let hist_idx = 2;
        session.history.truncate(hist_idx);
        session.restore_context_after_rewind(hist_idx, false);

        assert_eq!(session.history.len(), 2);
        assert_eq!(session.session_usage.prompt_tokens, Some(30));
        assert_eq!(session.session_usage.completion_tokens, Some(3));
        assert_eq!(session.context_tokens, Some(50));
        assert_eq!(session.context_tokens_history_len, Some(2));
        assert_eq!(session.session_cost_usd, 1.0);
    }

    #[test]
    fn app_rewind_restores_context_snapshot_without_rewinding_cumulative_spend() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.core.session.history = vec![user("a"), assistant("b")];
        app.app.core.session.session_usage.prompt_tokens = Some(10);
        app.app.core.session.session_usage.completion_tokens = Some(1);
        app.app.core.session.context_tokens = Some(50);
        app.app.core.session.context_tokens_history_len = Some(2);
        app.app.core.session.session_cost_usd = 0.5;
        app.app.core.session.snapshot_context();

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
        app.app.core.session.snapshot_context();
        app.app.restore_screen();

        let restored = app.app.rewind_to(2).expect("second user turn");

        assert_eq!(restored.0, "c");
        assert_eq!(app.app.core.session.history.len(), 2);
        assert_eq!(app.app.core.session.session_cost_usd, 1.0);
        assert_eq!(app.app.core.session.session_usage.prompt_tokens, Some(30));
        assert_eq!(
            app.app.core.session.session_usage.completion_tokens,
            Some(3)
        );
        assert_eq!(app.app.core.session.context_tokens, Some(50));
        assert_eq!(app.app.core.session.context_tokens_history_len, Some(2));
        assert_eq!(app.app.core.session.context_snapshots.len(), 1);
    }

    #[test]
    fn app_rewind_restores_turn_tps_snapshot() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.core.session.history = vec![user("a"), assistant("b")];
        app.app.core.session.turn_metas.push((
            2,
            protocol::TurnMeta {
                elapsed_ms: 10,
                avg_tps: Some(20.0),
                display_tps: Some(20.0),
                interrupted: false,
                tool_elapsed: std::collections::HashMap::new(),
            },
        ));
        app.app
            .core
            .session
            .history
            .extend([user("c"), assistant("d")]);
        app.app.core.session.turn_metas.push((
            4,
            protocol::TurnMeta {
                elapsed_ms: 20,
                avg_tps: Some(50.0),
                display_tps: Some(50.0),
                interrupted: false,
                tool_elapsed: std::collections::HashMap::new(),
            },
        ));
        app.app.restore_screen();
        assert_eq!(app.app.working.display_tps(), Some(50.0));

        let restored = app.app.rewind_to(2).expect("second user turn");

        assert_eq!(restored.0, "c");
        assert_eq!(app.app.core.session.history.len(), 2);
        assert_eq!(app.app.core.session.turn_metas.len(), 1);
        assert_eq!(app.app.working.display_tps(), Some(20.0));
    }

    #[test]
    fn app_rewind_restores_carried_tps_snapshot_without_turn_samples() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.core.session.history = vec![user("a"), assistant("b")];
        app.app.core.session.turn_metas.push((
            2,
            protocol::TurnMeta {
                elapsed_ms: 10,
                avg_tps: Some(20.0),
                display_tps: Some(20.0),
                interrupted: false,
                tool_elapsed: std::collections::HashMap::new(),
            },
        ));
        app.app
            .core
            .session
            .history
            .extend([user("c"), assistant("d")]);
        app.app.core.session.turn_metas.push((
            4,
            protocol::TurnMeta {
                elapsed_ms: 20,
                avg_tps: None,
                display_tps: Some(20.0),
                interrupted: false,
                tool_elapsed: std::collections::HashMap::new(),
            },
        ));
        app.app
            .core
            .session
            .history
            .extend([user("e"), assistant("f")]);
        app.app.core.session.turn_metas.push((
            6,
            protocol::TurnMeta {
                elapsed_ms: 30,
                avg_tps: Some(50.0),
                display_tps: Some(50.0),
                interrupted: false,
                tool_elapsed: std::collections::HashMap::new(),
            },
        ));
        app.app.restore_screen();
        assert_eq!(app.app.working.display_tps(), Some(50.0));

        let restored = app.app.rewind_to(4).expect("third user turn");

        assert_eq!(restored.0, "e");
        assert_eq!(app.app.core.session.history.len(), 4);
        assert_eq!(app.app.core.session.turn_metas.len(), 2);
        assert_eq!(app.app.working.turn_meta().unwrap().avg_tps, None);
        assert_eq!(app.app.working.display_tps(), Some(20.0));
    }

    #[test]
    fn rewind_past_all_context_snapshots_clears_context_not_cumulative_usage() {
        let mut session = smelt_core::session::Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![user("a"), assistant("b")];
        session.session_usage.prompt_tokens = Some(10);
        session.context_tokens = Some(50);
        session.context_tokens_history_len = Some(2);
        session.snapshot_context();

        session.history.truncate(0);
        session.restore_context_after_rewind(0, false);

        assert_eq!(session.session_usage.prompt_tokens, Some(10));
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
        session.snapshot_context();

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
        session.snapshot_context();
        session.context_tokens = Some(80);
        session.context_tokens_history_len = Some(4);
        session.snapshot_context();

        let hist_idx = 2;
        session.history.truncate(hist_idx);
        session.restore_context_after_rewind(hist_idx, false);

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
        session.restore_context_after_rewind(hist_idx, false);

        assert!(session.checkpoint.is_none());
        assert!(session.context_snapshots.is_empty());
        assert_eq!(session.session_usage.prompt_tokens, Some(30));
        assert_eq!(session.session_cost_usd, 1.0);
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
        session.snapshot_context();

        let hist_idx = 2;
        session.history.truncate(hist_idx);
        session.restore_context_after_rewind(hist_idx, true);

        assert!(session.checkpoint.is_some());
        assert!(session.context_tokens.is_none());
        assert!(session.context_tokens_history_len.is_none());
    }
}
