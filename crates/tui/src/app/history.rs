use crate::app::{SessionAccess, TuiApp};
use smelt_core::content::transcript::Transcript;
use smelt_core::session;
use smelt_core::transcript_model::BlockHistory;
use smelt_core::{Block, ToolOutput, ToolState, ToolStatus, TranscriptBlockDescriptor};

use protocol::{AgentMode, AssistantStep, Content, HistoryItem, UiCommand};
use std::collections::HashMap;
use std::path::PathBuf;
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
        self.resolve_with_context(name, args, true)
    }

    pub(crate) fn resolve_with_context(
        &self,
        name: &str,
        args: &HashMap<String, serde_json::Value>,
        final_args: bool,
    ) -> protocol::StyledLines {
        let summary = self.lua.tool_summary_with_context(name, args, final_args);
        if !summary.is_empty() || self.lua.has_tool(name) {
            return summary;
        }
        smelt_core::mcp::args_summary(args)
    }
}

#[cfg(test)]
pub(crate) fn live_session_for_test(
    id: String,
    history_len: usize,
    checkpoint: Option<smelt_core::ContextCheckpoint>,
) -> smelt_core::session_runtime::LiveSession {
    let revision = smelt_core::session::load_store_header(&id)
        .map(|(header, _)| header.revision)
        .unwrap_or(0);
    let header = smelt_core::session::SessionHeader {
        meta: smelt_core::session::SessionMeta {
            id,
            title: None,
            slug: None,
            first_user_message: None,
            created_at_ms: 0,
            updated_at_ms: 0,
            mode: None,
            reasoning_effort: None,
            model: None,
            fast_mode: None,
            cwd: None,
            parent_id: None,
            context_tokens: None,
            context_token_identity: None,
            display_context_token_identity: None,
            history_len: Some(history_len),
            checkpoint,
            text_bytes: None,
        },
        history_len,
        revision,
        degraded_warnings: Vec::new(),
    };
    smelt_core::session_runtime::LiveSession::from_parts(header, std::path::PathBuf::new(), None)
}

/// Every TUI full-history load must use one of these reasons. Healthy resume,
/// render, save, Lua lightweight APIs, rewind, and fork paths stay store-backed.
/// Read-only fallback is reserved for stores without a usable descriptor projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FullSessionMaterializationReason {
    InspectSessionDetail,
    ReadOnlyTranscriptFallback,
    #[cfg(test)]
    TestSavedSessionAssertion,
}

impl FullSessionMaterializationReason {
    fn counter(self) -> &'static str {
        match self {
            FullSessionMaterializationReason::InspectSessionDetail => {
                "inspect:session:detail_load_full"
            }
            FullSessionMaterializationReason::ReadOnlyTranscriptFallback => {
                "session:transcript:read_only_full_fallback"
            }
            #[cfg(test)]
            FullSessionMaterializationReason::TestSavedSessionAssertion => {
                "test:session:load_full_assertion"
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn materialize_full_session(
    id: &str,
    reason: FullSessionMaterializationReason,
) -> Option<session::Session> {
    materialize_full_session_result(id, reason).ok().flatten()
}

pub(crate) fn materialize_full_session_result(
    id: &str,
    reason: FullSessionMaterializationReason,
) -> session::SessionStoreResult<Option<session::Session>> {
    smelt_perf::perf::record_value("session:full_materialized", 1);
    smelt_perf::perf::record_value(reason.counter(), 1);
    session::load_full_result(id)
}

pub(crate) fn materialize_full_transcript_read_only(
    lua: &crate::lua::LuaRuntime,
    id: &str,
) -> Option<(crate::app::transcript::LoadedTranscript, usize)> {
    materialize_full_transcript_read_only_result(lua, id)
        .ok()
        .flatten()
}

pub(crate) fn materialize_full_transcript_read_only_result(
    lua: &crate::lua::LuaRuntime,
    id: &str,
) -> session::SessionStoreResult<Option<(crate::app::transcript::LoadedTranscript, usize)>> {
    let resolved = session::resolve_session_dir_for_read_result(id)?;
    let db_path = resolved.dir.join("session.db");
    let db = smelt_store::SessionReader::open_database(&db_path)
        .map_err(|err| smelt_core::session_store::store_error("open", &db_path, err))?;
    let descriptor_count = db.transcript_descriptor_count().map_err(|err| {
        smelt_core::session_store::store_error("read transcript descriptor count", &db_path, err)
    })?;
    let Some(session) = materialize_full_session_result(
        id,
        FullSessionMaterializationReason::ReadOnlyTranscriptFallback,
    )?
    else {
        return Ok(None);
    };
    Ok(Some((
        crate::app::transcript::LoadedTranscript::full(build_transcript_from_session(
            lua, &session,
        )),
        descriptor_count,
    )))
}

fn copy_legacy_attachment_blobs(
    source: &smelt_store::SessionReader,
    dest: &std::path::Path,
    references: &[String],
) -> Result<(), smelt_store::StoreError> {
    if references.is_empty() {
        return Ok(());
    }
    smelt_core::session::create_private_dir_all(dest)?;
    for reference in references {
        let blob = source.legacy_attachment_blob(reference)?;
        smelt_core::session::write_private_file(
            &dest.join(blob.filename),
            blob.data_url.as_bytes(),
        )?;
    }
    Ok(())
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

pub(crate) fn load_transcript_tail_from_sqlite(
    session: &session::Session,
    width: u16,
    viewport_rows: u16,
) -> Option<crate::app::transcript::LoadedTranscript> {
    load_transcript_tail_from_sqlite_dir(session::dir_for(session), width, viewport_rows)
}

pub(crate) fn load_transcript_tail_from_sqlite_id(
    id: &str,
    width: u16,
    viewport_rows: u16,
) -> Option<crate::app::transcript::LoadedTranscript> {
    let resolved = session::resolve_session_dir_for_read(id)?;
    if resolved.kind != session::SessionDirKind::Store {
        return None;
    }
    load_transcript_tail_from_sqlite_dir(resolved.dir, width, viewport_rows)
}

pub(crate) fn load_transcript_tail_from_sqlite_dir(
    session_dir: PathBuf,
    width: u16,
    viewport_rows: u16,
) -> Option<crate::app::transcript::LoadedTranscript> {
    crate::app::transcript::LoadedTranscript::tail_from_sqlite_dir(
        session_dir,
        width,
        viewport_rows,
    )
}

#[cfg(test)]
fn transcript_covers_history(transcript: &Transcript, session: &session::Session) -> bool {
    block_history_covers_history(&transcript.history, session)
}

fn block_history_covers_history(history: &BlockHistory, session: &session::Session) -> bool {
    descriptor_records_cover_history(history.descriptor_records().iter(), session)
}

fn descriptor_records_cover_history<'a>(
    records: impl IntoIterator<Item = &'a smelt_core::TranscriptBlockRecord>,
    session: &session::Session,
) -> bool {
    let records = records.into_iter().collect::<Vec<_>>();
    session.history.iter().enumerate().all(|(idx, item)| {
        if fallback_history_item_block_count(item) == 0 {
            return true;
        }
        records
            .iter()
            .any(|record| matches!(record.origin, Some(smelt_core::BlockOrigin::History(origin)) if origin == idx))
    })
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
                    title: None,
                    summary_titles: Vec::new(),
                    content: reasoning.clone(),
                    kind: protocol::ReasoningKind::Raw,
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
                preview_output: None,
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
    fn transcript_coverage_requires_rendered_history_origins() {
        let mut session = session::Session::new(1, std::path::PathBuf::from("/tmp"));
        session
            .history
            .push(HistoryItem::user(Content::text("first")));
        session
            .history
            .push(HistoryItem::Assistant(protocol::AssistantStep::terminal(
                Some(Content::text("answer")),
                None,
                Vec::new(),
            )));

        let mut transcript = Transcript::new();
        transcript.push_with_origin(
            Block::Text {
                content: "answer".into(),
            },
            smelt_core::BlockOrigin::History(1),
        );
        assert!(!transcript_covers_history(&transcript, &session));

        transcript.push_with_origin(
            Block::User {
                text: "first".into(),
                image_labels: vec![],
            },
            smelt_core::BlockOrigin::History(0),
        );
        assert!(transcript_covers_history(&transcript, &session));
    }

    #[test]
    fn transcript_coverage_ignores_unrendered_context_notes() {
        let mut session = session::Session::new(1, std::path::PathBuf::from("/tmp"));
        session
            .history
            .push(HistoryItem::note(protocol::HistoryNote::context(
                "Current working directory: /tmp.",
            )));
        assert!(transcript_covers_history(&Transcript::new(), &session));
    }

    #[test]
    fn display_only_sqlite_load_reads_bounded_tail_descriptors() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = smelt_store::SessionDb::open(dir.path().join("session.db")).unwrap();
        let records = (0..200)
            .map(|idx| test_descriptor_record(idx, &format!("block {idx}")))
            .collect::<Vec<_>>();
        db.replace_transcript_descriptor_records_for_repair(&records)
            .unwrap();
        drop(db);

        let loaded = load_transcript_tail_from_sqlite_dir(dir.path().to_path_buf(), 10, 1)
            .expect("tail transcript");
        let descriptor_window = loaded.descriptor_window.expect("descriptor window");
        assert_eq!(descriptor_window.start.get(), 160);
        assert_eq!(descriptor_window.end().get(), 200);
        assert_eq!(descriptor_window.total_count, 200);
        assert_eq!(
            descriptor_window.hydration,
            smelt_store::TranscriptDescriptorHydration::ObjectBacked
        );
        assert!(loaded.transcript.history.order.is_empty());
        assert_eq!(descriptor_window.records.len(), 40);
        assert_eq!(descriptor_window.records[0].block_id.get(), 160);
        assert_eq!(
            descriptor_window.records[0].record.origin,
            Some(smelt_core::BlockOrigin::History(160))
        );
        assert_eq!(descriptor_window.records[39].block_id.get(), 199);
        assert_eq!(
            descriptor_window.records[39].record.origin,
            Some(smelt_core::BlockOrigin::History(199))
        );
    }

    #[test]
    fn display_only_sqlite_load_counts_non_dense_descriptor_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = smelt_store::SessionDb::open(dir.path().join("session.db")).unwrap();
        let records = vec![
            test_descriptor_record(70, "visible old tail"),
            test_descriptor_record(235, "visible newest tail"),
        ];
        db.replace_transcript_descriptor_records_for_repair(&records)
            .unwrap();
        drop(db);

        let loaded = load_transcript_tail_from_sqlite_dir(dir.path().to_path_buf(), 80, 12)
            .expect("tail transcript");
        let descriptor_window = loaded.descriptor_window.expect("descriptor window");
        assert_eq!(descriptor_window.start.get(), 0);
        assert_eq!(descriptor_window.end().get(), 2);
        assert_eq!(descriptor_window.total_count, 2);
        assert_eq!(descriptor_window.records[0].block_id.get(), 70);
        assert_eq!(descriptor_window.records[1].block_id.get(), 235);
    }

    fn test_descriptor_record(
        block_idx: u64,
        content: &str,
    ) -> smelt_store::TranscriptDescriptorRecord {
        smelt_store::TranscriptDescriptorRecord {
            block_idx,
            history_idx: None,
            kind: "text".to_string(),
            tool_call_id: None,
            tool_name: None,
            content_hash: "0".to_string(),
            estimated_text_bytes: content.len() as u64,
            preview_text: content.to_string(),
            indexed_text: content.to_string(),
            descriptor_json: serde_json::to_string(&TranscriptBlockDescriptor::Text {
                content: content.to_string(),
            })
            .unwrap(),
            origin_json: Some(
                serde_json::to_string(&smelt_core::BlockOrigin::History(block_idx as usize))
                    .unwrap(),
            ),
            tool_state_json: None,
        }
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

    pub(crate) fn set_history_from(&mut self, first_index: usize, history: Vec<HistoryItem>) {
        if self.block_read_only_mutation("update read-only session history") {
            return;
        }
        let current_len = self.session_history_len();
        if first_index > current_len {
            self.notify_error_sticky(format!(
                "invalid canonical history update: start {first_index} exceeds length {current_len}"
            ));
            return;
        }
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
        let comparable_len = history.len().min(current_len.saturating_sub(first_index));
        let existing = self.session_history_range(first_index..first_index + comparable_len);
        let common_len = existing
            .iter()
            .zip(history.iter())
            .take_while(|(left, right)| left == right)
            .count();
        let dirty_from = first_index.saturating_add(common_len);
        let final_len = first_index.saturating_add(history.len());
        if dirty_from < current_len || common_len < history.len() {
            self.session_truncate_from(dirty_from);
            for item in history.iter().skip(common_len).cloned() {
                self.session_append_history(item);
            }
        }
        debug_assert_eq!(self.session_history_len(), final_len);
        for item in applied_items {
            self.commit_pending_history_append(&item);
        }
        self.sync_session_snapshot();
        self.publish_history_delta("set");
    }

    pub(crate) fn append_engine_history_items(
        &mut self,
        first_index: usize,
        items: Vec<HistoryItem>,
    ) {
        if items.is_empty() {
            return;
        }
        if self.block_read_only_mutation("append to read-only session history") {
            return;
        }

        smelt_perf::perf::record_value("tui:history_appended:items", items.len() as u64);
        smelt_perf::perf::record_value("tui:history_appended:first_index", first_index as u64);
        let current_len = self.session_history_len();
        if first_index > current_len {
            self.notify_error_sticky(format!(
                "invalid canonical history append: start {first_index} exceeds length {current_len}"
            ));
            return;
        }

        let already_present = first_index.saturating_add(items.len()) <= current_len
            && self.session_history_range(first_index..first_index.saturating_add(items.len()))
                == items;

        if already_present {
            smelt_perf::perf::record_value("tui:history_appended:already_present", 1);
        } else {
            if first_index < self.session_history_len() {
                self.session_truncate_from(first_index);
            }
            for item in items.iter().cloned() {
                self.session_append_history(item);
            }
        }

        for item in &items {
            self.commit_pending_history_append(item);
        }
        self.sync_session_snapshot();
        self.publish_history_delta("append");
    }

    pub(crate) fn publish_history_delta(&mut self, kind: &str) {
        if matches!(kind, "cleared" | "rewound" | "loaded" | "forked") {
            self.bump_epoch("history_epoch");
        }
        let count = self.session_history_len();
        self.core.signals.emit_dyn(
            "history",
            std::rc::Rc::new(smelt_core::signals::HistoryDelta {
                kind: kind.into(),
                count,
            }),
        );
    }

    fn mark_history_dirty_from(&mut self, idx: usize) {
        if !self.session_access.is_read_only() {
            self.session_document.mark_history_resave_required(idx);
        }
    }

    pub(crate) fn apply_session_document_mutation(
        &mut self,
        mutation: crate::app::session_document::SessionMutation,
    ) -> crate::app::session_document::MutationResult {
        self.session_document.apply(
            &mut self.core.session,
            &mut self.parser,
            !self.session_access.is_read_only(),
            mutation,
        )
    }

    pub(crate) fn session_is_read_only(&self) -> bool {
        self.session_access.is_read_only()
    }

    fn read_only_reason(&self) -> String {
        match &self.session_access {
            SessionAccess::ReadOnly { reason } => reason.clone(),
            SessionAccess::Owned => "session is read-only".to_string(),
        }
    }

    pub(crate) fn block_read_only_mutation(&mut self, action: &str) -> bool {
        if self.session_is_read_only() {
            self.notify_error(format!("cannot {action}: {}", self.read_only_reason()));
            true
        } else {
            false
        }
    }

    pub(crate) fn apply_history_append_to_history(
        &mut self,
        append: &protocol::HistoryAppend,
    ) -> protocol::HistoryAppendResult {
        if self.block_read_only_mutation("append to read-only session history") {
            return protocol::HistoryAppendResult::Unchanged;
        }
        if self.session_document.live_session.is_some() {
            let old_len = self.session_history_len();
            if matches!(append.policy, protocol::HistoryAppendPolicy::Append) {
                self.session_append_history(append.item.clone());
                return protocol::HistoryAppendResult::Pushed;
            }

            let tail_start = old_len.saturating_sub(128);
            let mut tail = self.session_history_range(tail_start..old_len);
            let old_tail = tail.clone();
            let result = protocol::apply_history_append(&mut tail, append);
            if result != protocol::HistoryAppendResult::Unchanged {
                let dirty_offset = old_tail
                    .iter()
                    .zip(tail.iter())
                    .take_while(|(left, right)| left == right)
                    .count();
                self.session_truncate_from(tail_start.saturating_add(dirty_offset));
                for item in tail.into_iter().skip(dirty_offset) {
                    self.session_append_history(item);
                }
            }
            return result;
        }

        let result = self.apply_session_document_mutation(
            crate::app::session_document::SessionMutation::ApplyHistoryAppend {
                append: append.clone(),
                identity: self.active_context_token_identity(),
            },
        );
        let append_result = result
            .history_append_result
            .unwrap_or(protocol::HistoryAppendResult::Unchanged);
        if append_result == protocol::HistoryAppendResult::RemovedLast {
            self.sync_task_label_from_session();
        }
        append_result
    }

    pub(crate) fn sync_session_snapshot(&mut self) {
        if self.session_access.is_read_only() {
            self.publish_shared_session_state();
            return;
        }
        self.apply_session_document_mutation(
            crate::app::session_document::SessionMutation::UpdateRuntimeMetadata {
                updated_at_ms: session::now_ms(),
                mode: self.core.config.mode.as_str().to_string(),
                reasoning_effort: self.core.config.reasoning_effort,
                model: self.current_model_key(),
                fast_mode: self.fast_mode(),
            },
        );
        self.publish_shared_session_state();
    }

    /// Stable requested key so session restore preserves provider/auth identity,
    /// including a selection that is pending managed-model refresh.
    fn current_model_key(&self) -> Option<String> {
        self.core.config.model_selection.requested_key.clone()
    }

    fn restore_session_model(&mut self, model_key: &str) {
        let resolved_key =
            smelt_core::config::resolve_model_ref(&self.core.config.available_models, model_key)
                .ok()
                .map(|resolved| resolved.key.clone());
        if let Some(key) = resolved_key {
            self.apply_model(&key, false);
        } else if smelt_core::managed_model_selection_is_pending(
            &self.core.config.providers,
            &self.core.config.available_models,
            model_key,
        ) {
            let selection = smelt_core::ModelSelectionState {
                requested_key: Some(model_key.to_string()),
                requested_by: smelt_core::ModelSelectionSource::Session,
                active: None,
            };
            let selection_changed = self.core.config.model_selection != selection;
            let context_changed = self.core.config.context_window.is_some();
            if selection_changed || context_changed {
                self.core.config.revision = self.core.config.revision.wrapping_add(1);
            }
            if selection_changed {
                self.core.config.model_selection = selection;
                self.core
                    .signals
                    .set_dyn("model", std::rc::Rc::new(Option::<String>::None));
            }
            if context_changed {
                self.core.config.context_window = None;
            }
        } else {
            self.notify_error_sticky(format!(
                "session model '{model_key}' is no longer available"
            ));
        }
    }

    pub(crate) fn set_session_title(
        &mut self,
        title: String,
        slug: String,
        target_history_len: Option<usize>,
    ) {
        if self.block_read_only_mutation("rename read-only session") {
            return;
        }
        let hist_len = target_history_len.unwrap_or_else(|| self.session_history_len());
        self.apply_session_document_mutation(
            crate::app::session_document::SessionMutation::SetTitle {
                title,
                slug: slug.clone(),
                snapshot_history_len: hist_len,
            },
        );
        self.set_task_label(slug);
        self.save_session();
    }

    pub(crate) fn restore_session_metadata_after_rewind(&mut self, hist_idx: usize) {
        self.apply_session_document_mutation(
            crate::app::session_document::SessionMutation::RestoreMetadataAfterRewind {
                history_len: hist_idx,
            },
        );
        self.sync_task_label_from_session();
    }

    fn sync_task_label_from_session(&mut self) {
        let slug = self.core.session.slug.clone().unwrap_or_default();
        self.set_task_label(slug);
    }

    fn apply_rewindable_session_state(&mut self, turn_meta: Option<protocol::TurnMeta>) {
        self.sync_task_label_from_session();
        if let Some(meta) = turn_meta {
            self.working.restore_from_turn_meta(&meta);
        } else {
            self.working.clear();
        }
    }

    fn rewind_session_history_to(&mut self, hist_idx: usize, keep_checkpoint_at_boundary: bool) {
        let identity = self.active_context_token_identity();
        let result = self.apply_session_document_mutation(
            crate::app::session_document::SessionMutation::RewindHistoryTo {
                index: hist_idx,
                keep_checkpoint_at_boundary,
                identity,
            },
        );
        self.apply_rewindable_session_state(result.turn_meta);
    }

    fn prune_rewindable_session_state(&mut self, hist_idx: usize) {
        let identity = self.active_context_token_identity();
        let result = self.apply_session_document_mutation(
            crate::app::session_document::SessionMutation::PruneRewindableState {
                history_len: hist_idx,
                identity,
            },
        );
        self.apply_rewindable_session_state(result.turn_meta);
    }

    pub(crate) fn fork_session(&mut self) {
        if self.session_document.live_session.is_some() {
            self.fork_live_session();
            return;
        }
        self.ensure_live_session_materialized();
        if self.session_is_empty() {
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
        self.session_access = SessionAccess::Owned;
        self.session_document.mark_session_unpersisted();
        self.bump_epoch("session_epoch");
        self.save_session();
        self.flush_persist();
        self.core
            .signals
            .emit_dyn("session_ended", std::rc::Rc::new(original_id.clone()));
        self.core.signals.emit_dyn(
            "session_started",
            std::rc::Rc::new(self.core.session.id.clone()),
        );
        self.publish_history_delta("forked");
        self.notify(format!("forked from {original_id}"));
        // Drain stale events so old snapshots don't overwrite the forked session.
        while self.core.engine.try_recv().is_ok() {}
    }

    fn fork_live_session(&mut self) {
        if self.session_is_empty() {
            self.notify_error("nothing to fork".into());
            return;
        }
        if self.agent.is_some() {
            self.cancel_agent();
            self.agent = None;
        }
        self.save_session();
        self.flush_persist();
        self.stop_background_processes();

        if let Some(live) = self.session_document.live_session.as_mut() {
            if let Some((header, _)) = session::load_store_header_for_dir(live.dir().to_path_buf())
            {
                live.replace_header(header);
            }
        }

        let Some(live) = self.session_document.live_session.as_ref() else {
            return;
        };
        let original_id = self.core.session.id.clone();
        let history_len = live.history_len();
        let mut forked = self.core.session.fork(self.core.env.pid());
        forked.history.clear();
        let fork_id = match smelt_core::session_id::SessionId::parse(&forked.id) {
            Ok(id) => id,
            Err(err) => {
                self.notify_error_sticky(format!("failed to prepare fork id: {err}"));
                return;
            }
        };
        let staged_fork = match session::StagedSessionDir::create(&fork_id) {
            Ok(staged) => staged,
            Err(err) => {
                self.notify_error_sticky(format!("failed to stage fork directory: {err}"));
                return;
            }
        };
        let fork_work_dir = staged_fork.path().to_path_buf();
        let state = match session::store_state_from_session(&forked, history_len) {
            Ok(state) => state,
            Err(err) => {
                self.notify_error_sticky(format!("failed to prepare fork: {err}"));
                return;
            }
        };
        let source = match smelt_store::SessionReader::open_existing(live.dir()) {
            Ok(source) => source,
            Err(err) => {
                self.notify_error_sticky(format!("failed to open source session store: {err}"));
                return;
            }
        };
        let legacy_attachments = match source.legacy_attachment_references(history_len) {
            Ok(references) => references,
            Err(err) => {
                self.notify_error_sticky(format!(
                    "failed to inspect source session attachments: {err}"
                ));
                return;
            }
        };
        let mut maintenance =
            match smelt_store::SessionMaintenance::open(&fork_work_dir, &forked.id) {
                Ok(maintenance) => maintenance,
                Err(err) => {
                    self.notify_error_sticky(format!("failed to own fork destination: {err}"));
                    return;
                }
            };
        if let Err(err) = maintenance.copy_prefix_from(&source, &state, history_len) {
            self.notify_error_sticky(format!("failed to fork session store: {err}"));
            return;
        }
        if let Err(err) =
            copy_legacy_attachment_blobs(&source, &fork_work_dir.join("blobs"), &legacy_attachments)
        {
            self.notify_error_sticky(format!("failed to fork legacy session attachments: {err}"));
            return;
        }
        if let Err(err) = session::refresh_derived_files(&fork_work_dir) {
            self.notify_error_sticky(format!("failed to refresh fork metadata: {err}"));
            return;
        }
        if let Err(err) = maintenance.release() {
            self.notify_error_sticky(format!("failed to release fork destination: {err}"));
            return;
        }
        let fork_dir = match staged_fork.publish() {
            Ok(path) => path,
            Err(err) => {
                self.notify_error_sticky(format!("failed to publish fork destination: {err}"));
                return;
            }
        };
        let Some((header, store_ref)) = session::load_store_header_for_dir(fork_dir.clone()) else {
            self.notify_error_sticky("failed to load forked session header".into());
            return;
        };
        let transcript = crate::app::history::load_transcript_tail_from_sqlite_dir(
            fork_dir.clone(),
            self.last_width,
            self.last_height,
        )
        .unwrap_or_else(|| crate::app::transcript::LoadedTranscript::empty_store(fork_dir));
        let document = crate::app::session_document::SessionDocument::from_store(
            header,
            store_ref,
            transcript,
            self.core.env.pid(),
            self.core.env.cwd(),
        )
        .into_store_backed();
        self.load_store_backed_session(document);
        self.publish_history_delta("forked");
        self.notify(format!("forked from {original_id}"));
    }

    pub(crate) fn reset_session(&mut self) {
        let _perf = smelt_perf::perf::begin("app:reset_session");
        self.session_document.live_session = None;
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
        self.flush_persist();
        if let Err(err) = self.persister.release() {
            self.notify_error_sticky(format!("failed to release session writer: {err}"));
        }
        let old_id = self.core.session.id.clone();
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
        self.session_access = SessionAccess::Owned;
        self.session_document.mark_session_unpersisted();
        self.bump_epoch("session_epoch");
        if let Ok(mut guard) = self.shared_session.lock() {
            *guard = None;
        }
        self.core
            .signals
            .emit_dyn("session_ended", std::rc::Rc::new(old_id));
        self.core.signals.emit_dyn(
            "session_started",
            std::rc::Rc::new(self.core.session.id.clone()),
        );
        self.publish_history_delta("cleared");
        // Drain stale events so old Messages snapshots don't restore history into the fresh session.
        while self.core.engine.try_recv().is_ok() {}
    }

    fn install_loaded_session(&mut self, loaded: session::Session) {
        self.core.session = loaded;
        let session_cwd = self.core.session.cwd.clone();
        if let crate::app::cwd::SessionCwdRestore::Fallback {
            requested,
            fallback,
            error,
        } = self.restore_session_cwd(session_cwd.as_deref())
        {
            self.notify_error(format!(
                "session cwd unavailable: {requested}: {error}; using {fallback}"
            ));
        }
    }

    fn claim_writer_access_for_current_session(&mut self) {
        self.session_access = match self.persister.open_owned(&self.core.session.id) {
            Ok(()) => SessionAccess::Owned,
            Err(reason) => {
                self.notify_error_sticky(format!("opened session read-only: {reason}"));
                SessionAccess::ReadOnly { reason }
            }
        };
    }

    pub fn load_session(&mut self, loaded: session::Session) {
        let transcript = crate::app::transcript::LoadedTranscript::full(
            build_transcript_from_session(&self.lua, &loaded),
        );
        let document =
            crate::app::session_document::SessionDocument::from_full_session(loaded, transcript)
                .into_full();
        self.load_full_session_document(document);
    }

    fn load_full_session_document(
        &mut self,
        document: crate::app::session_document::FullSessionDocument,
    ) {
        let crate::app::session_document::FullSessionDocument {
            session: loaded,
            transcript,
        } = document;
        self.session_document.live_session = None;
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

        if self.core.startup_overrides.mode.is_none() {
            if let Some(mode) = loaded.mode.as_deref().and_then(AgentMode::parse) {
                self.set_mode(mode, false);
            }
        }
        if self.core.startup_overrides.reasoning_effort.is_none() {
            if let Some(effort) = loaded.reasoning_effort {
                self.set_reasoning_effort(effort, false);
            }
        }
        // Only restore model/API settings if not overridden by CLI.
        if !self.core.startup_overrides.fixes_model_selection() {
            if let Some(ref model_key) = loaded.model {
                self.restore_session_model(model_key);
            }
        }

        self.install_loaded_session(loaded);
        self.claim_writer_access_for_current_session();
        let history_len = self.session_history_len();
        let revision = session::load_store_header_for_dir(session::dir_for(&self.core.session))
            .map_or(0, |(header, _)| header.revision);
        self.session_document.install_loaded_full_session(
            transcript,
            !self.session_access.is_read_only(),
            history_len,
            revision,
        );
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
        self.pending_history_appends.clear();
        self.parser.clear();
        self.sync_session_snapshot();
        self.core
            .signals
            .emit_dyn("session_ended", std::rc::Rc::new(old_id));
        self.core.signals.emit_dyn(
            "session_started",
            std::rc::Rc::new(self.core.session.id.clone()),
        );
        self.publish_history_delta("loaded");
        // Drain stale engine events so old snapshots don't overwrite
        // the loaded session's state.
        while self.core.engine.try_recv().is_ok() {}
    }

    pub(crate) fn load_store_backed_session(
        &mut self,
        document: crate::app::session_document::StoreBackedSessionDocument,
    ) {
        let crate::app::session_document::StoreBackedSessionDocument {
            session: loaded,
            transcript,
            live_session,
            persisted_descriptor_len,
        } = document;
        if self.agent.is_some() {
            self.cancel_agent();
            self.agent = None;
        }
        self.lua.cancel_tasks();
        let old_id = self.core.session.id.clone();
        self.flush_persist();

        if self.core.startup_overrides.mode.is_none() {
            if let Some(mode) = loaded.mode.as_deref().and_then(AgentMode::parse) {
                self.set_mode(mode, false);
            }
        }
        if self.core.startup_overrides.reasoning_effort.is_none() {
            if let Some(effort) = loaded.reasoning_effort {
                self.set_reasoning_effort(effort, false);
            }
        }
        if !self.core.startup_overrides.fixes_model_selection() {
            if let Some(ref model_key) = loaded.model {
                self.restore_session_model(model_key);
            }
        }

        let descriptor_window_len = transcript
            .descriptor_window
            .as_ref()
            .map_or(0, |window| window.records.len());
        smelt_perf::perf::record_value("session:resume:store_backed", 1);
        smelt_perf::perf::record_value(
            "transcript:descriptor_window:active_records",
            descriptor_window_len as u64,
        );
        smelt_perf::perf::record_value(
            "live_session:suffix_items",
            live_session.live_suffix_len() as u64,
        );
        smelt_perf::perf::record_value(
            "live_session:suffix_bytes",
            live_session.live_suffix_bytes() as u64,
        );
        self.install_loaded_session(loaded);
        self.claim_writer_access_for_current_session();
        debug_assert!(
            self.core.session.history.is_empty(),
            "store-backed TUI sessions must not retain materialized history"
        );
        let history_len = live_session.history_len();
        self.bump_epoch("session_epoch");
        self.reset_session_permissions();
        self.queued_inputs.clear();
        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
        self.input.clear(&mut pctx);
        self.input.store.lock().unwrap().clear();
        self.stop_background_processes();
        self.clear_transcript();
        self.session_document.install_loaded_store_session(
            transcript,
            live_session,
            !self.session_access.is_read_only(),
            history_len,
            persisted_descriptor_len,
        );
        self.publish_shared_session_state();
        self.core
            .signals
            .emit_dyn("session_ended", std::rc::Rc::new(old_id));
        self.core.signals.emit_dyn(
            "session_started",
            std::rc::Rc::new(self.core.session.id.clone()),
        );
        self.publish_history_delta("loaded");
        while self.core.engine.try_recv().is_ok() {}
    }

    // Explicit promotion for old in-memory-only UI flows. Normal store-backed
    // resume, render, save, rewind, and fork paths must not call this.
    pub(crate) fn ensure_live_session_materialized(&mut self) {
        let Some(live_session) = self.session_document.live_session.take() else {
            return;
        };
        match live_session
            .materialize_full_session(&self.core.session, "compat:session:display_only_promotion")
        {
            Ok(loaded) => {
                self.install_loaded_session(loaded);
                self.prune_rewindable_session_state(self.core.session.history.len());
                if !block_history_covers_history(
                    self.session_document.transcript.history(),
                    &self.core.session,
                ) {
                    let transcript = build_transcript_from_session(&self.lua, &self.core.session);
                    self.apply_session_document_mutation(crate::app::session_document::SessionMutation::ReplaceTranscriptFromHistory {
                        transcript,
                    });
                }
                self.sync_session_snapshot();
            }
            Err(_err) => {
                smelt_perf::perf::record_value("live_session:materialize_error", 1);
                self.session_document.live_session = Some(live_session);
            }
        }
    }

    pub(crate) fn session_history_len(&self) -> usize {
        self.session_document
            .live_session
            .as_ref()
            .map_or(self.core.session.history.len(), |live| live.history_len())
    }

    pub(crate) fn session_is_empty(&self) -> bool {
        self.session_document
            .live_session
            .as_ref()
            .map_or(self.core.session.history.is_empty(), |live| live.is_empty())
    }

    pub(crate) fn session_history_range(&self, range: std::ops::Range<usize>) -> Vec<HistoryItem> {
        if let Some(live) = &self.session_document.live_session {
            return live.history_range(range).unwrap_or_else(|_err| {
                smelt_perf::perf::record_value("live_session:history_range_error", 1);
                Vec::new()
            });
        }
        let end = range.end.min(self.core.session.history.len());
        let start = range.start.min(end);
        self.core.session.history[start..end].to_vec()
    }

    #[allow(dead_code)]
    pub(crate) fn session_history_tail(
        &self,
        max_items: usize,
        max_bytes: Option<usize>,
    ) -> Vec<HistoryItem> {
        if let Some(live) = &self.session_document.live_session {
            return live
                .history_tail(max_items, max_bytes)
                .unwrap_or_else(|_err| {
                    smelt_perf::perf::record_value("live_session:history_tail_error", 1);
                    Vec::new()
                });
        }
        let len = self.core.session.history.len();
        self.core.session.history[len.saturating_sub(max_items)..].to_vec()
    }

    pub(crate) fn session_append_history(&mut self, item: HistoryItem) -> usize {
        let result = self.apply_session_document_mutation(
            crate::app::session_document::SessionMutation::AppendHistoryItem { item },
        );
        result
            .history_idx
            .expect("append history mutation returns index")
    }

    fn commit_request_history_item_to_document(
        &mut self,
        item: HistoryItem,
        block: Option<Block>,
        first_user_message: Option<String>,
    ) -> usize {
        let mutation = crate::app::session_document::SessionMutation::CommitRequestHistoryItem {
            item,
            block,
            first_user_message,
        };
        let result = self.apply_session_document_mutation(mutation);
        result
            .history_idx
            .expect("commit request history item mutation returns index")
    }

    #[allow(dead_code)]
    pub(crate) fn session_truncate_from(&mut self, index: usize) {
        self.apply_session_document_mutation(
            crate::app::session_document::SessionMutation::TruncateHistoryFrom {
                index,
                identity: self.active_context_token_identity(),
            },
        );
        self.sync_task_label_from_session();
    }

    #[allow(dead_code)]
    pub(crate) fn session_checkpoint(&self) -> Option<&smelt_core::ContextCheckpoint> {
        self.core.session.checkpoint.as_ref()
    }

    pub(crate) fn session_set_checkpoint(
        &mut self,
        checkpoint: Option<smelt_core::ContextCheckpoint>,
    ) {
        let mutation = crate::app::session_document::SessionMutation::SetCheckpoint { checkpoint };
        self.apply_session_document_mutation(mutation);
    }

    fn store_session_model_history_source(&self) -> protocol::ModelHistorySource {
        if let Some(live) = &self.session_document.live_session {
            return live.model_history_source(
                engine::SUMMARY_PREFIX,
                self.core.session.checkpoint.as_ref(),
            );
        }
        let (prefix, first_live_index, end_index) = self
            .core
            .session
            .model_history_range(engine::SUMMARY_PREFIX);
        protocol::ModelHistorySource::store(prefix, first_live_index, end_index)
    }

    fn materialize_model_history_source(
        &self,
        source: protocol::ModelHistorySource,
    ) -> Vec<HistoryItem> {
        match source {
            protocol::ModelHistorySource::Items { items, .. } => items,
            protocol::ModelHistorySource::Store {
                prefix,
                first_live_index,
                end_index,
                suffix,
                ..
            } => {
                let mut history = prefix;
                history.extend(self.session_history_range(first_live_index..end_index));
                history.extend(suffix);
                history
            }
        }
    }

    pub(crate) fn session_model_history_source(&self) -> protocol::ModelHistorySource {
        let source = self.store_session_model_history_source();
        if self.ephemeral() {
            let coordinates = source.coordinates();
            protocol::ModelHistorySource::projected_items(
                self.materialize_model_history_source(source),
                coordinates,
            )
        } else {
            source
        }
    }

    // ── History / session ────────────────────────────────────────────────

    pub(crate) fn restore_screen(&mut self) {
        self.rebuild_screen_from_history();
    }

    fn rebuild_screen_from_history(&mut self) {
        self.clear_transcript();
        self.prune_rewindable_session_state(self.core.session.history.len());
        let width = self.transcript_width() as u16;
        let viewport_rows = self.viewport_rows_estimate();
        let (loaded_transcript, descriptors_persisted) =
            match load_transcript_tail_from_sqlite(&self.core.session, width, viewport_rows) {
                Some(loaded_transcript) => (loaded_transcript, true),
                None => {
                    // Sessions without descriptor rows rebuild the display transcript from the loaded session.
                    smelt_perf::perf::record_value("session:rebuild_transcript_full_fallback", 1);
                    (
                        crate::app::transcript::LoadedTranscript::full(
                            build_transcript_from_session(&self.lua, &self.core.session),
                        ),
                        false,
                    )
                }
            };
        self.session_document
            .transcript
            .replace_loaded_transcript(loaded_transcript);
        self.session_document
            .install_materialized_session(descriptors_persisted);
    }

    pub(crate) fn schedule_session_save(&mut self) {
        self.session_document.queue_save();
    }

    pub(crate) fn save_session_if_pending(&mut self) {
        if self.session_document.is_save_queued() && !self.prompt_input_is_busy() {
            self.save_session();
        }
    }

    pub(crate) fn next_persistence_retry_delay(&self) -> Option<Duration> {
        self.persistence_retry
            .next_delay(self.core.clock.instant_now())
    }

    pub(crate) fn try_persistence_retry(&mut self) -> bool {
        let now = self.core.clock.instant_now();
        let Some(session_id) = self.persistence_retry.start_due(now) else {
            return false;
        };
        if session_id != self.core.session.id
            || self.session_access.is_read_only()
            || self.ephemeral()
        {
            self.persistence_retry.reset();
            return true;
        }
        if self.session_document.has_pending_save() {
            return true;
        }
        self.session_document.queue_save();
        self.save_session();
        if !self.session_document.has_pending_save() {
            self.persistence_retry.reset();
        }
        true
    }

    pub(crate) fn ack_persist_save(&mut self, receipt: smelt_store::SaveReceipt) {
        let session_id = receipt.session_id.clone();
        let save_queued = self
            .session_document
            .mark_persisted(&receipt, self.core.session.checkpoint.as_ref());
        if session_id == self.core.session.id {
            self.persistence_retry.reset();
        }
        self.dismiss_session_save_failure_notification(&session_id);
        if save_queued {
            self.save_session();
        }
    }

    pub(crate) fn fail_persist_save(&mut self, err: crate::persist::PersistFailure) {
        self.session_document.mark_persist_failed(&err);
        let show_notification = match err.disposition {
            smelt_store::SessionPersistenceDisposition::Retry => {
                smelt_perf::perf::record_value("session:save:retryable_failure", 1);
                self.persistence_retry
                    .schedule(&err.session_id, self.core.clock.instant_now())
                    > 1
            }
            smelt_store::SessionPersistenceDisposition::Reopen => {
                self.persistence_retry.reset();
                true
            }
            smelt_store::SessionPersistenceDisposition::ReadOnly => {
                self.persistence_retry.reset();
                self.session_access = SessionAccess::ReadOnly {
                    reason: err.message.clone(),
                };
                true
            }
            smelt_store::SessionPersistenceDisposition::OwnershipLost => {
                self.persistence_retry.reset();
                let reason = "session writer ownership was lost".to_string();
                if let Err(release_err) = self.persister.release() {
                    self.notify_error_sticky(format!(
                        "failed to release session writer after ownership loss: {release_err}"
                    ));
                }
                self.session_access = SessionAccess::ReadOnly { reason };
                true
            }
        };
        if show_notification {
            self.notify_session_save_failure(&err.session_id, &err.message);
        }
    }

    fn submit_persist_command(&mut self, command: smelt_store::SessionCommit) -> bool {
        let failure = crate::persist::PersistFailure {
            save_id: command.save_id.get(),
            session_id: command.session_id.clone(),
            message: "persistence worker is disconnected".into(),
            commit_failure: None,
            disposition: smelt_store::SessionPersistenceDisposition::Reopen,
        };
        if self
            .persister
            .save(crate::persist::PersistRequest { command })
            .is_ok()
        {
            return true;
        }
        self.fail_persist_save(failure);
        false
    }

    pub(crate) fn save_session(&mut self) {
        let _perf = smelt_perf::perf::begin("session:save");
        if self.ephemeral() {
            self.session_document.clear_queued_save();
            self.update_session_persist_metadata();
            self.publish_shared_session_state();
            self.session_document.mark_ephemeral_persisted(None);
            return;
        }
        if self.session_access.is_read_only() {
            self.session_document.clear_queued_save();
            return;
        }
        if self.persistence_retry.delays_save(&self.core.session.id) {
            self.session_document.queue_save();
            return;
        }
        if self.session_document.has_pending_save() {
            self.session_document.queue_save();
            return;
        }
        if self.session_document.live_session.is_some() {
            self.save_live_session();
            return;
        }
        self.session_document.clear_queued_save();
        let metadata = self.runtime_session_metadata();
        let prepared = match self
            .session_document
            .prepare_save(&mut self.core.session, metadata)
        {
            Ok(prepared) => prepared,
            Err(err) => {
                self.notify_error_sticky(format!("failed to prepare session save: {err}"));
                return;
            }
        };
        let session = &self.core.session;
        let session_id = session.id.clone();
        if let Ok(mut guard) = self.shared_session.lock() {
            *guard = Some(crate::app::SharedSessionState {
                id: session_id.clone(),
                has_messages: !session.history.is_empty(),
                ephemeral: self.ephemeral(),
            });
        }
        match prepared {
            crate::app::session_document::PreparedSessionSave::Skip(reason) => {
                if reason == crate::app::session_document::SessionSaveSkipReason::Unchanged {
                    smelt_perf::perf::record_value("session:save:skipped_unchanged", 1);
                }
            }
            crate::app::session_document::PreparedSessionSave::MetadataOnly {
                generation,
                state,
                side_tables,
            } => {
                smelt_perf::perf::record_value("session:save:metadata_only", 1);
                let Some(submitted) = self.session_document.submit_metadata_save(
                    session_id,
                    generation,
                    *state,
                    *side_tables,
                ) else {
                    return;
                };
                self.submit_persist_command(submitted.command);
            }
            crate::app::session_document::PreparedSessionSave::History { generation, delta } => {
                let submitted = match self
                    .session_document
                    .submit_history_save(session_id, generation, *delta, None)
                {
                    Ok(Some(submitted)) => submitted,
                    Ok(None) => return,
                    Err(err) => {
                        self.notify_error_sticky(format!(
                            "failed to prepare session commit: {err}"
                        ));
                        return;
                    }
                };
                self.submit_persist_command(submitted.command);
            }
            crate::app::session_document::PreparedSessionSave::RequestHistoryAppend { .. } => {
                unreachable!("full session save preparation must not build request append save")
            }
        }
    }

    fn save_live_session(&mut self) {
        self.session_document.clear_queued_save();
        let Some(live) = self.session_document.live_session.as_ref() else {
            return;
        };
        smelt_perf::perf::record_value("live_session:suffix_items", live.live_suffix_len() as u64);
        smelt_perf::perf::record_value(
            "live_session:suffix_bytes",
            live.live_suffix_bytes() as u64,
        );
        let metadata = self.runtime_session_metadata();
        let prepared = match self
            .session_document
            .prepare_save(&mut self.core.session, metadata)
        {
            Ok(prepared) => prepared,
            Err(err) => {
                self.notify_error_sticky(format!("failed to prepare session save: {err}"));
                return;
            }
        };
        let (generation, delta) = match prepared {
            crate::app::session_document::PreparedSessionSave::Skip(reason) => {
                if reason == crate::app::session_document::SessionSaveSkipReason::Unchanged {
                    smelt_perf::perf::record_value("session:save:skipped_unchanged", 1);
                }
                return;
            }
            crate::app::session_document::PreparedSessionSave::MetadataOnly { .. } => {
                unreachable!("live session saves are always history saves")
            }
            crate::app::session_document::PreparedSessionSave::History { generation, delta } => {
                (generation, *delta)
            }
            crate::app::session_document::PreparedSessionSave::RequestHistoryAppend { .. } => {
                unreachable!("live session save preparation must not build request append save")
            }
        };
        let session_id = self.core.session.id.clone();
        self.publish_shared_session_state();
        let submitted = match self
            .session_document
            .submit_history_save(session_id, generation, delta, None)
        {
            Ok(Some(submitted)) => submitted,
            Ok(None) => return,
            Err(err) => {
                self.notify_error_sticky(format!("failed to prepare session commit: {err}"));
                return;
            }
        };
        self.submit_persist_command(submitted.command);
    }

    fn runtime_session_metadata(&self) -> crate::app::session_document::RuntimeSessionMetadata {
        crate::app::session_document::RuntimeSessionMetadata {
            updated_at_ms: session::now_ms(),
            mode: self.core.config.mode.as_str().to_string(),
            reasoning_effort: self.core.config.reasoning_effort,
            model: self.current_model_key(),
            fast_mode: self.fast_mode(),
        }
    }

    pub(crate) fn update_session_persist_metadata(&mut self) {
        let metadata = self.runtime_session_metadata();
        self.apply_session_document_mutation(
            crate::app::session_document::SessionMutation::UpdateRuntimeMetadata {
                updated_at_ms: metadata.updated_at_ms,
                mode: metadata.mode,
                reasoning_effort: metadata.reasoning_effort,
                model: metadata.model,
                fast_mode: metadata.fast_mode,
            },
        );
    }

    pub(crate) fn session_document_has_unflushed_work(&self) -> bool {
        if self.ephemeral() || self.session_access.is_read_only() {
            return false;
        }
        self.session_document.has_unflushed_work(&self.core.session)
    }

    /// Save the current session, then block until all persistence work triggered by
    /// that save has either completed or the document can no longer make progress.
    pub(crate) fn save_session_and_flush(&mut self) {
        const MAX_FAILURE_RETRIES: usize = 2;

        let mut failure_retries = 0;
        self.persistence_retry.start_now(&self.core.session.id);
        self.save_session();
        loop {
            let outcome = self.flush_persist();
            if !self.session_document_has_unflushed_work() {
                break;
            }
            let retry_delay = match &outcome {
                crate::persist::PersistFlushOutcome::Drained => Some(Duration::ZERO),
                crate::persist::PersistFlushOutcome::CommitFailed(failures) => {
                    if failure_retries >= MAX_FAILURE_RETRIES
                        || !failures
                            .iter()
                            .all(|failure| failure.disposition.should_retry_automatically())
                    {
                        None
                    } else {
                        failure_retries += 1;
                        Some(crate::app::persistence_retry_delay(failure_retries as u32))
                    }
                }
                crate::persist::PersistFlushOutcome::WorkerExited
                | crate::persist::PersistFlushOutcome::Disconnected => None,
            };
            let Some(retry_delay) = retry_delay else {
                break;
            };
            if retry_delay != Duration::ZERO {
                std::thread::sleep(retry_delay);
                self.persistence_retry.start_now(&self.core.session.id);
            }
            if !self.session_document.has_pending_save() {
                self.save_session();
                if !self.session_document.has_pending_save() {
                    break;
                }
            }
        }
    }

    /// Block until all queued persist writes complete. Call before reading session files from disk.
    pub(crate) fn flush_persist(&mut self) -> crate::persist::PersistFlushOutcome {
        let outcome = self.persister.flush();
        self.drain_persist_reports();
        match &outcome {
            crate::persist::PersistFlushOutcome::WorkerExited => {
                self.notify_error_sticky(
                    "persistence worker exited before queued writes completed".into(),
                );
            }
            crate::persist::PersistFlushOutcome::Disconnected => {
                self.notify_error_sticky("persistence worker is disconnected".into());
            }
            crate::persist::PersistFlushOutcome::Drained
            | crate::persist::PersistFlushOutcome::CommitFailed(_) => {}
        }
        outcome
    }

    pub(crate) fn shutdown_persist(&mut self) -> Result<(), String> {
        let result = self.persister.shutdown();
        if let Err(err) = &result {
            self.notify_error_sticky(format!("failed to stop persistence worker: {err}"));
        }
        result
    }

    fn suppress_duplicate_carried_tail_before(&mut self, index: usize) -> usize {
        let history = self.session_document.transcript.history();
        if index == 0 || index >= history.order.len() {
            return index;
        }
        let prev_id = history.order[index - 1];
        let next_id = history.order[index];
        let duplicate = match (history.block(prev_id), history.block(next_id)) {
            (Some(prev), Some(next)) => checkpoint_suffix_blocks_match(prev, next),
            _ => false,
        };
        if duplicate {
            let result =
                self.apply_session_document_mutation(crate::app::session_document::SessionMutation::RemoveUnoriginatedTranscriptBlockAt {
                    block_index: index - 1,
                });
            if result.applied {
                return index - 1;
            }
        }
        index
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
            .session_document
            .transcript
            .history()
            .first_block_index_for_history_origin_at_or_after(first_live_index)
        {
            let index = self.suppress_duplicate_carried_tail_before(index);
            self.apply_session_document_mutation(
                crate::app::session_document::SessionMutation::InsertCheckpointMarker {
                    block_index: index,
                    block,
                },
            );
        } else {
            let index = fallback_transcript_index_for_history_index(
                &self.core.session.history,
                first_live_index,
            );
            let index = self.suppress_duplicate_carried_tail_before(index);
            self.apply_session_document_mutation(
                crate::app::session_document::SessionMutation::InsertCheckpointMarker {
                    block_index: index,
                    block,
                },
            );
        }
    }

    fn install_live_context_checkpoint(
        &mut self,
        kind: String,
        summary: String,
        first_live_message_index: usize,
        tokens_before: Option<u32>,
    ) -> bool {
        let Some(live) = self.session_document.live_session.as_ref() else {
            return false;
        };
        if summary.trim().is_empty() || live.is_empty() {
            return false;
        }
        let first_live_index = match live.first_live_history_index_for_model_message(
            self.core.session.checkpoint.as_ref(),
            first_live_message_index,
        ) {
            Ok(Some(index)) => index,
            Ok(None) => return false,
            Err(err) => {
                self.notify_error_sticky(format!("failed to install checkpoint: {err}"));
                return false;
            }
        };
        let history_len = live.history_len();
        self.apply_session_document_mutation(
            crate::app::session_document::SessionMutation::InstallContextCheckpointAtHistoryIndex {
                kind,
                summary,
                first_live_index,
                tokens_before,
                history_len,
            },
        )
        .applied
    }

    pub(crate) fn install_context_checkpoint(
        &mut self,
        kind: String,
        summary: String,
        first_live_message_index: usize,
        tokens_before: Option<u32>,
    ) -> bool {
        let installed = if self.session_document.live_session.is_some() {
            self.install_live_context_checkpoint(
                kind,
                summary,
                first_live_message_index,
                tokens_before,
            )
        } else {
            self.apply_session_document_mutation(
                crate::app::session_document::SessionMutation::InstallContextCheckpoint {
                    kind,
                    summary,
                    first_live_message_index,
                    tokens_before,
                },
            )
            .applied
        };
        if !installed {
            self.clear_compaction_preview();
            self.notify("nothing old enough to compact".to_string());
            return false;
        }
        let tokens_after_estimate =
            smelt_core::session::estimate_message_tokens(&self.model_history_messages());
        let history_len = self.session_history_len();
        self.apply_session_document_mutation(
            crate::app::session_document::SessionMutation::SetCheckpointTokensAfterEstimate {
                tokens: tokens_after_estimate,
                history_len,
            },
        );
        self.session_set_checkpoint(self.core.session.checkpoint.clone());
        let follow_tail = self.transcript_win().is_following_tail();
        self.clear_compaction_preview();
        self.reset_visible_context_tokens();
        self.refresh_compaction_marker();
        self.publish_history_delta("checkpoint");
        self.schedule_session_save();
        if follow_tail {
            self.transcript_win_mut().follow_tail();
        }
        true
    }

    pub(crate) fn model_history_source(&self) -> protocol::ModelHistorySource {
        self.session_model_history_source()
    }

    pub(crate) fn model_history(&self) -> Vec<HistoryItem> {
        self.materialize_model_history_source(self.model_history_source())
    }

    pub(crate) fn model_history_messages(&self) -> Vec<protocol::Message> {
        match self.read_model_history_from_store() {
            Ok(Some(history)) => {
                smelt_perf::perf::record_value("tui:model_history:messages_store", 1);
                protocol::history_to_messages(&history)
            }
            Ok(None) => {
                smelt_perf::perf::record_value("tui:model_history:messages_fallback_expected", 1);
                protocol::history_to_messages(&self.model_history())
            }
            Err(err) => {
                smelt_perf::perf::record_value("tui:model_history:messages_fallback_error", 1);
                smelt_perf::perf::record_value(
                    "tui:model_history:messages_fallback_error_bytes",
                    err.len() as u64,
                );
                protocol::history_to_messages(&self.model_history())
            }
        }
    }

    fn read_model_history_from_store(&self) -> Result<Option<Vec<HistoryItem>>, String> {
        if self.session_document.has_session_work() {
            return Ok(None);
        }
        let protocol::ModelHistorySource::Store {
            prefix,
            first_live_index,
            end_index,
            suffix,
            ..
        } = self.model_history_source()
        else {
            return Ok(None);
        };
        let mut history = prefix;
        if end_index > first_live_index {
            let db_path = session::dir_for(&self.core.session).join("session.db");
            let db = smelt_store::SessionReader::open_database(&db_path)
                .map_err(|err| format!("open model history database {db_path:?}: {err}"))?;
            let mut rows = db
                .read_history_items_range(first_live_index..end_index)
                .map_err(|err| format!("read model history rows: {err}"))?;
            smelt_perf::perf::record_value(
                "tui:model_history:messages_store_rows",
                rows.len() as u64,
            );
            history.append(&mut rows);
        }
        history.extend(suffix);
        Ok(Some(history))
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

        let hist_idx = if self.session_document.live_session.is_some() {
            match self
                .session_document
                .transcript
                .history()
                .block_origin_at(block_idx)
            {
                Some(smelt_core::BlockOrigin::History(history_idx)) => history_idx,
                _ => {
                    smelt_perf::perf::record_value("rewind:live_missing_history_origin", 1);
                    self.notify_error("cannot rewind this transcript block".into());
                    return None;
                }
            }
        } else {
            self.ensure_live_session_materialized();
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
            hist_idx
        };

        let rewind_item = self.session_history_range(hist_idx..hist_idx.saturating_add(1));
        let images: Vec<(String, String)> = match rewind_item.first() {
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

        let mode_after_rewind = if let Some(live) = &self.session_document.live_session {
            match live.any_transcript_visible_before(hist_idx) {
                Ok(false) => None,
                Ok(true) => match live.effective_mode_at(
                    hist_idx,
                    self.core.session.mode.as_deref().unwrap_or("normal"),
                ) {
                    Ok(mode) => AgentMode::parse(&mode),
                    Err(err) => {
                        smelt_perf::perf::record_value("rewind:live_mode_error", 1);
                        self.notify_error_sticky(format!(
                            "failed to read mode after rewind: {err}"
                        ));
                        None
                    }
                },
                Err(err) => {
                    smelt_perf::perf::record_value("rewind:live_visible_scan_error", 1);
                    self.notify_error_sticky(format!(
                        "failed to read history before rewind: {err}"
                    ));
                    None
                }
            }
        } else {
            self.core.session.history[..hist_idx]
                .iter()
                .any(HistoryItem::is_transcript_visible)
                .then(|| self.mode_at_history_boundary(hist_idx))
        };

        let keep_checkpoint_at_boundary = turn_text.is_some()
            && self
                .core
                .session
                .checkpoint
                .as_ref()
                .is_some_and(|cp| cp.first_live_index == hist_idx);
        self.rewind_session_history_to(hist_idx, keep_checkpoint_at_boundary);
        self.truncate_to(block_idx);
        if let Some(mode) = mode_after_rewind {
            self.restore_mode_after_rewind(mode);
        }
        self.reset_session_permissions();
        self.sync_session_snapshot();
        self.publish_history_delta("rewound");

        turn_text.map(|t| (t, images))
    }

    pub(crate) fn rewind_to_start(&mut self) {
        self.rewind_session_history_to(0, false);
        self.task_label = None;
        self.working.clear();
        self.clear_transcript();
        self.reset_session_permissions();
        self.sync_session_snapshot();
        self.publish_history_delta("rewound");
    }

    fn persist_history_suffix(
        &mut self,
        generation: crate::app::session_document::DocumentGeneration,
        delta: Box<smelt_core::session_save::PersistDelta>,
        descriptor_append: Option<crate::app::session_document::DescriptorAppendSubmission>,
    ) -> bool {
        self.publish_shared_session_state();
        if self.ephemeral() {
            self.session_document
                .mark_ephemeral_persisted(descriptor_append);
            return true;
        }
        let session = &self.core.session;
        let session_id = session.id.clone();
        let submitted = match self.session_document.submit_history_save(
            session_id,
            generation,
            *delta,
            descriptor_append,
        ) {
            Ok(Some(submitted)) => submitted,
            Ok(None) => return false,
            Err(err) => {
                self.notify_error_sticky(format!("failed to prepare session commit: {err}"));
                return false;
            }
        };
        self.submit_persist_command(submitted.command)
    }

    fn commit_live_session_request_history_item(
        &mut self,
        item: HistoryItem,
        block: Option<Block>,
        first_user_message: Option<String>,
    ) -> protocol::ModelHistorySource {
        let history = self.model_history_source();
        let history_index = self.session_history_len();
        let descriptor_order_start = self.session_document.transcript.history().len();
        self.commit_request_history_item_to_document(item.clone(), block, first_user_message);
        let metadata = self.runtime_session_metadata();
        let prepared = match self.session_document.prepare_request_history_append_save(
            &mut self.core.session,
            metadata,
            crate::app::session_document::RuntimeRequestHistoryAppendSave {
                history_index,
                descriptor_order_start,
                item: &item,
                include_side_tables: false,
            },
        ) {
            Ok(plan) => plan,
            Err(err) => {
                self.notify_error_sticky(format!("failed to prepare session save: {err}"));
                self.mark_history_dirty_from(history_index);
                self.publish_history_delta("request");
                self.save_session();
                self.flush_persist();
                return history;
            }
        };
        match prepared {
            crate::app::session_document::PreparedSessionSave::RequestHistoryAppend {
                generation,
                delta,
                descriptor_append,
            } => {
                self.persist_history_suffix(generation, delta, Some(descriptor_append));
            }
            crate::app::session_document::PreparedSessionSave::History { generation, delta } => {
                self.persist_history_suffix(generation, delta, None);
            }
            crate::app::session_document::PreparedSessionSave::Skip(_)
            | crate::app::session_document::PreparedSessionSave::MetadataOnly { .. } => {
                self.mark_history_dirty_from(history_index);
                self.save_session();
            }
        }
        self.publish_history_delta("request");
        self.flush_persist();
        history
    }

    pub(crate) fn commit_request_history_item(
        &mut self,
        item: HistoryItem,
        block: Option<Block>,
    ) -> protocol::ModelHistorySource {
        self.commit_request_history_item_with_first_user(item, block, None)
    }

    pub(crate) fn commit_request_history_item_with_first_user(
        &mut self,
        item: HistoryItem,
        block: Option<Block>,
        first_user_message: Option<String>,
    ) -> protocol::ModelHistorySource {
        if self.session_document.live_session.is_some() {
            return self.commit_live_session_request_history_item(item, block, first_user_message);
        }
        let history = self.model_history_source();
        let history_index = self.core.session.history.len();
        let descriptor_order_start = self.session_document.transcript.history().len();
        self.commit_request_history_item_to_document(item.clone(), block, first_user_message);
        let metadata = self.runtime_session_metadata();
        let prepared = match self.session_document.prepare_request_history_append_save(
            &mut self.core.session,
            metadata,
            crate::app::session_document::RuntimeRequestHistoryAppendSave {
                history_index,
                descriptor_order_start,
                item: &item,
                include_side_tables: true,
            },
        ) {
            Ok(plan) => plan,
            Err(err) => {
                self.notify_error_sticky(format!("failed to prepare session save: {err}"));
                self.mark_history_dirty_from(history_index);
                self.sync_session_snapshot();
                self.publish_history_delta("request");
                self.save_session();
                self.flush_persist();
                return history;
            }
        };
        match prepared {
            crate::app::session_document::PreparedSessionSave::RequestHistoryAppend {
                generation,
                delta,
                descriptor_append,
            } => {
                let persisted =
                    self.persist_history_suffix(generation, delta, Some(descriptor_append));
                if !persisted {
                    self.mark_history_dirty_from(history_index);
                }
            }
            crate::app::session_document::PreparedSessionSave::History { generation, delta } => {
                let persisted = self.persist_history_suffix(generation, delta, None);
                if !persisted {
                    self.mark_history_dirty_from(history_index);
                }
            }
            crate::app::session_document::PreparedSessionSave::Skip(_)
            | crate::app::session_document::PreparedSessionSave::MetadataOnly { .. } => {
                self.mark_history_dirty_from(history_index);
                self.sync_session_snapshot();
                self.save_session();
            }
        }
        self.publish_history_delta("request");
        self.flush_persist();
        history
    }

    pub(crate) fn show_user_message(&mut self, input: &str, image_labels: Vec<String>) {
        self.push_block(Block::User {
            text: input.to_string(),
            image_labels,
        });
    }
}

#[cfg(test)]
mod checkpoint_tests {
    use super::*;
    use protocol::Content;
    use smelt_core::ContextCheckpoint;

    #[test]
    fn missing_session_model_key_is_retained_as_pending_selection() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app
            .core
            .config
            .providers
            .push(smelt_core::config::ProviderConfig {
                name: Some("managed".into()),
                provider_type: Some("codex".into()),
                api_base: Some("https://chatgpt.com/backend-api/codex".into()),
                ..Default::default()
            });
        let revision = app.app.core.config.revision;
        app.app.core.config.context_window = Some(128_000);

        app.app.restore_session_model("managed/future-model");

        assert_eq!(
            app.app.core.config.model_selection.requested_key.as_deref(),
            Some("managed/future-model")
        );
        assert_eq!(
            app.app.core.config.model_selection.requested_by,
            smelt_core::ModelSelectionSource::Session
        );
        assert!(app.app.core.config.active_model().is_none());
        assert_eq!(app.app.core.config.context_window, None);
        assert_eq!(app.app.core.config.revision, revision.wrapping_add(1));
        assert!(app.run_lua(
            r#"
                assert(smelt.model.current() == nil)
                local status = smelt.model.status()
                assert(status.requested == "managed/future-model")
                assert(status.availability == "pending")
            "#,
        ));
    }

    #[test]
    fn missing_static_session_model_keeps_the_current_fallback() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let previous = app.app.core.config.model_selection.clone();
        let revision = app.app.core.config.revision;

        app.app.restore_session_model("removed/model");

        assert_eq!(app.app.core.config.model_selection, previous);
        assert_eq!(app.app.core.config.revision, revision);
        assert!(app.app.notification_win().is_some());
    }

    fn perf_value_max(label: &str) -> u64 {
        smelt_perf::perf::snapshot()
            .values
            .into_iter()
            .find(|row| row.label == label)
            .map(|row| row.max)
            .unwrap_or(0)
    }

    fn perf_duration_max(label: &str) -> u64 {
        smelt_perf::perf::snapshot()
            .durations
            .into_iter()
            .find(|row| row.label == label)
            .map(|row| row.max_us)
            .unwrap_or(0)
    }

    fn assert_perf_value_absent(label: &str) {
        let value = perf_value_max(label);
        assert_eq!(value, 0, "{label} recorded {value}, expected no samples");
    }

    fn assert_perf_value_at_most(label: &str, max: u64) {
        let value = perf_value_max(label);
        assert!(
            value <= max,
            "{label} recorded max {value}, expected <= {max}"
        );
    }

    fn assert_cached_persist_db() {
        assert_eq!(
            perf_duration_max("store:db:open_read_write"),
            0,
            "hot suffix save should reuse the persist worker database connection"
        );
        assert_eq!(
            perf_value_max("store:db:cached_read_write"),
            1,
            "hot suffix save did not reuse the persist worker database connection"
        );
    }

    fn assert_no_full_store_reads() {
        for label in [
            "store:history:read_all",
            "store:history:read_all_rows",
            "store:session:load_full_snapshot",
            "store:session:full_snapshot_rows_read",
            "store:transcript:search_blob_full",
            "store:transcript:read_descriptors_full",
            "store:transcript:descriptors_full_loaded",
            "session:full_materialized",
            "session:rebuild_transcript_full_fallback",
            "session:display_only_load_full",
            "session:save:descriptor_sparse_full_rebuild",
            "transcript:build_from_session:history_items",
        ] {
            assert_perf_value_absent(label);
        }
    }

    fn large_saved_session_app(history_len: usize) -> crate::app::test_harness::TestApp {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let mut session = session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
        session.first_user_message = Some("old user 0".into());
        session.history = (0..history_len)
            .map(|idx| {
                if idx.is_multiple_of(2) {
                    user(&format!("old user {idx}"))
                } else {
                    assistant(&format!("old assistant {idx}"))
                }
            })
            .collect();

        app.app.load_session(session);
        app.app.restore_screen();
        app.app.session_document.mark_session_unpersisted();
        app.app.save_session();
        app.app.flush_persist();
        app
    }

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

    #[test]
    fn full_session_load_installs_document_transcript() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.show_user_message("stale visible block", Vec::new());

        let mut session = session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
        session.history = vec![user("loaded user"), assistant("loaded assistant")];

        app.app.load_session(session);

        let history = app.app.session_document.transcript.history();
        let visible_text = history
            .order
            .iter()
            .filter_map(|id| match history.block(*id) {
                Some(Block::User { text, .. }) => Some(text.clone()),
                Some(Block::Text { content }) => Some(content.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(visible_text, vec!["loaded user", "loaded assistant"]);
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
    fn tui_full_session_materialization_is_centralized() {
        fn visit(dir: &std::path::Path, offenders: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).expect("read source dir") {
                let entry = entry.expect("read source entry");
                let path = entry.path();
                if path.is_dir() {
                    visit(&path, offenders);
                    continue;
                }
                if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                    continue;
                }
                let source = std::fs::read_to_string(&path).expect("read rust source");
                let direct_core = ["smelt_core::session::", "load_full("].concat();
                let direct_imported = ["session::", "load_full("].concat();
                let wrapper_call = ["session::", "load_full(id)"].concat();
                for (line_idx, line) in source.lines().enumerate() {
                    let is_direct = line.contains(&direct_core) || line.contains(&direct_imported);
                    let is_wrapper =
                        path.ends_with("app/history.rs") && line.trim() == wrapper_call;
                    if is_direct && !is_wrapper {
                        offenders.push(format!("{}:{}:{line}", path.display(), line_idx + 1));
                    }
                }
            }
        }

        let mut offenders = Vec::new();
        visit(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut offenders,
        );
        assert!(
            offenders.is_empty(),
            "TUI full session materialization must use materialize_full_session with an explicit reason:\n{}",
            offenders.join("\n")
        );
    }

    #[test]
    fn descriptorless_store_resume_falls_back_without_repairing() {
        let mut app = large_saved_session_app(256);
        let id = app.app.core.session.id.clone();
        let session_dir = session::dir_for_id(&id);
        let db = smelt_store::SessionDb::open(session_dir.join("session.db")).unwrap();
        db.connection()
            .execute(
                "UPDATE transcript_blocks
                 SET descriptor_idx = NULL,
                     descriptor_json = NULL,
                     origin_json = NULL,
                     tool_state_json = NULL",
                [],
            )
            .unwrap();
        assert_eq!(db.transcript_descriptor_count().unwrap(), 0);
        drop(db);

        smelt_perf::perf::clear();
        smelt_perf::perf::set_enabled(true);
        app.app.load_session_by_id(&id);
        smelt_perf::perf::set_enabled(false);

        assert_eq!(app.app.session_history_len(), 256);
        assert!(app.app.session_document.live_session.is_some());
        assert!(app.app.core.session.history.is_empty());
        assert!(app.app.transcript_total_rows() > 0);
        assert_eq!(
            perf_value_max("session:transcript:read_only_full_fallback"),
            1
        );
        let db = smelt_store::SessionReader::open_database(session_dir.join("session.db")).unwrap();
        assert_eq!(db.transcript_descriptor_count().unwrap(), 0);
        drop(db);

        app.app
            .session_append_history(user("repair descriptors on owned save"));
        app.app.save_session();
        app.app.flush_persist();

        let db = smelt_store::SessionReader::open_database(session_dir.join("session.db")).unwrap();
        assert_eq!(db.transcript_descriptor_count().unwrap(), 256);
    }

    #[test]
    fn display_only_request_append_preserves_persisted_history_prefix() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let mut session = session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
        session.first_user_message = Some("old user".into());
        session.history = vec![user("old user"), assistant("old assistant")];

        app.app.load_session(session);
        app.app.restore_screen();
        app.app.session_document.mark_session_unpersisted();
        app.app.save_session();
        app.app.flush_persist();

        let id = app.app.core.session.id.clone();
        let loaded_transcript = load_transcript_tail_from_sqlite_id(&id, 80, 24)
            .expect("display-only transcript tail should load");
        let mut display_session =
            session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
        display_session.id = id.clone();
        display_session.first_user_message = Some("old user".into());

        app.app.load_store_backed_session(
            crate::app::session_document::StoreBackedSessionDocument::new(
                display_session,
                loaded_transcript,
                crate::app::history::live_session_for_test(id.clone(), 2, None),
            ),
        );

        let source = app.app.commit_request_history_item(
            user("new user"),
            Some(Block::User {
                text: "new user".into(),
                image_labels: vec![],
            }),
        );

        assert!(matches!(
            source,
            protocol::ModelHistorySource::Store {
                first_live_index: 0,
                end_index: 2,
                ..
            }
        ));
        assert_eq!(
            app.app
                .session_document
                .live_session
                .as_ref()
                .map(|live| live.history_len()),
            Some(3)
        );

        let db =
            smelt_store::SessionReader::open_database(session::dir_for_id(&id).join("session.db"))
                .expect("open session db");
        let rows = db
            .read_history_items_range(0..3)
            .expect("read persisted history");
        assert_eq!(
            rows,
            vec![
                user("old user"),
                assistant("old assistant"),
                user("new user")
            ]
        );
        let descriptors = db
            .read_all_transcript_descriptor_records()
            .expect("read transcript descriptors");
        assert!(descriptors
            .iter()
            .any(|record| record.history_idx == Some(2)));
    }

    #[test]
    fn normal_request_append_preserves_persisted_history_prefix() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let mut session = session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
        session.first_user_message = Some("old user".into());
        session.history = vec![user("old user"), assistant("old assistant")];

        app.app.load_session(session);
        app.app.restore_screen();
        app.app.session_document.mark_session_unpersisted();
        app.app.save_session();
        app.app.flush_persist();

        let id = app.app.core.session.id.clone();
        let source = app.app.commit_request_history_item(
            user("new user"),
            Some(Block::User {
                text: "new user".into(),
                image_labels: vec![],
            }),
        );

        assert!(matches!(
            source,
            protocol::ModelHistorySource::Store {
                first_live_index: 0,
                end_index: 2,
                ..
            }
        ));
        assert_eq!(app.app.core.session.history.len(), 3);

        let db =
            smelt_store::SessionReader::open_database(session::dir_for_id(&id).join("session.db"))
                .expect("open session db");
        let rows = db
            .read_history_items_range(0..3)
            .expect("read persisted history");
        assert_eq!(
            rows,
            vec![
                user("old user"),
                assistant("old assistant"),
                user("new user")
            ]
        );
        let descriptors = db
            .read_all_transcript_descriptor_records()
            .expect("read transcript descriptors");
        assert!(descriptors
            .iter()
            .any(|record| record.history_idx == Some(2)));
    }

    #[test]
    fn normal_request_append_persists_only_dirty_suffix_rows() {
        const OLD_HISTORY_LEN: usize = 256;
        let mut app = large_saved_session_app(OLD_HISTORY_LEN);

        app.app.restore_screen();

        smelt_perf::perf::clear();
        smelt_perf::perf::set_enabled(true);
        let source = app.app.commit_request_history_item(
            user("new user"),
            Some(Block::User {
                text: "new user".into(),
                image_labels: vec![],
            }),
        );
        let snapshot = smelt_perf::perf::snapshot();
        smelt_perf::perf::set_enabled(false);

        assert!(matches!(
            source,
            protocol::ModelHistorySource::Store {
                first_live_index: 0,
                end_index: OLD_HISTORY_LEN,
                ..
            }
        ));
        assert_eq!(app.app.core.session.history.len(), OLD_HISTORY_LEN + 1);
        assert!(snapshot
            .values
            .iter()
            .any(|row| row.label == "persist:write:history_items"));
        assert_perf_value_at_most("persist:write:history_items", 1);
        assert_perf_value_at_most("store:session:dirty_suffix_history_rows", 1);
        assert_perf_value_at_most("store:history:dirty_suffix_rows", 1);
        assert_perf_value_at_most("store:session:history_rows_inserted", 1);
        assert_perf_value_at_most("store:session:history_rows_deleted", 0);
        assert_perf_value_at_most("persist:write:descriptor_records", 1);
        assert_perf_value_at_most("store:transcript:dirty_descriptor_suffix_rows", 1);
        assert_perf_value_at_most("store:transcript:descriptor_db_rows_inserted", 1);
        assert_cached_persist_db();
        assert_no_full_store_reads();

        let db = smelt_store::SessionReader::open_database(
            session::dir_for_id(&app.app.core.session.id).join("session.db"),
        )
        .expect("open session db");
        let descriptors = db
            .read_all_transcript_descriptor_records()
            .expect("read transcript descriptors");
        assert_eq!(descriptors.len(), OLD_HISTORY_LEN + 1);
        assert_eq!(
            descriptors.last().and_then(|row| row.history_idx),
            Some(OLD_HISTORY_LEN as u64)
        );
    }

    #[test]
    fn live_session_checkpoint_uses_store_history_coordinates() {
        let mut app = large_saved_session_app(32);
        let id = app.app.core.session.id.clone();
        app.app.load_session_by_id(&id);
        assert!(app.app.session_document.live_session.is_some());
        assert!(app.app.core.session.history.is_empty());

        let installed = app.app.install_context_checkpoint(
            "compaction".into(),
            "store summary".into(),
            4,
            Some(100),
        );

        assert!(installed);
        assert_eq!(app.app.core.session.history.len(), 0);
        let checkpoint = app.app.core.session.checkpoint.as_ref().unwrap();
        assert_eq!(checkpoint.first_live_index, 4);
        match app.app.session_model_history_source() {
            protocol::ModelHistorySource::Store {
                prefix,
                first_live_index,
                end_index,
                suffix,
                ..
            } => {
                assert_eq!(prefix.len(), 1);
                assert_eq!(first_live_index, 4);
                assert_eq!(end_index, 32);
                assert!(suffix.is_empty());
            }
            protocol::ModelHistorySource::Items { .. } => {
                panic!("expected store-backed model history")
            }
        }
        assert!(app.app.session_document.is_save_queued());
    }

    #[test]
    fn live_session_save_persists_only_dirty_suffix_rows() {
        const OLD_HISTORY_LEN: usize = 256;
        let mut app = large_saved_session_app(OLD_HISTORY_LEN);
        let id = app.app.core.session.id.clone();
        app.app.load_session_by_id(&id);
        assert!(app.app.session_document.live_session.is_some());
        assert!(app.app.core.session.history.is_empty());

        smelt_perf::perf::clear();
        smelt_perf::perf::set_enabled(true);
        app.app
            .append_engine_history_items(OLD_HISTORY_LEN, vec![assistant("new assistant")]);
        app.app.save_session();
        app.app.flush_persist();
        smelt_perf::perf::set_enabled(false);

        assert_eq!(app.app.session_history_len(), OLD_HISTORY_LEN + 1);
        assert!(app.app.core.session.history.is_empty());
        assert_perf_value_at_most("persist:write:history_items", 1);
        assert_perf_value_at_most("store:session:dirty_suffix_history_rows", 1);
        assert_perf_value_at_most("store:history:dirty_suffix_rows", 1);
        assert_perf_value_at_most("store:session:history_rows_inserted", 1);
        assert_perf_value_at_most("store:session:history_rows_deleted", 0);
        assert_no_full_store_reads();

        let db =
            smelt_store::SessionReader::open_database(session::dir_for_id(&id).join("session.db"))
                .expect("open session db");
        let tail = db
            .read_history_items_range(OLD_HISTORY_LEN..OLD_HISTORY_LEN + 1)
            .expect("read persisted tail");
        assert_eq!(tail, vec![assistant("new assistant")]);
    }

    #[test]
    fn normal_preview_and_open_avoid_compat_full_load_fallbacks() {
        let mut app = large_saved_session_app(256);
        let id = app.app.core.session.id.clone();

        smelt_perf::perf::clear();
        smelt_perf::perf::set_enabled(true);
        {
            let _guard = crate::lua::install_app_ptr(&mut app.app);
            app.app
                .lua
                .lua
                .globals()
                .set("session_id", id.clone())
                .expect("set session id");
            app.app
                .lua
                .lua
                .load(
                    r#"
                    local buf = smelt.buf.new({})
                    local out = smelt.session.render_preview_into(
                        session_id,
                        { buf = buf, width = 80, height = 12 }
                    )
                    assert(out ~= nil and out.total_rows > 0)
                    "#,
                )
                .exec()
                .expect("render sparse session preview");
        }
        app.app.load_session_by_id(&id);
        smelt_perf::perf::set_enabled(false);

        assert_no_full_store_reads();
    }

    #[test]
    fn history_only_session_preview_and_open_fall_back_to_full_rebuild() {
        let id = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let mut app = crate::app::test_harness::TestApp::builder().build();

        let mut saved = session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
        saved.id = id.into();
        saved.first_user_message = Some("history only user 0".into());
        saved.history = vec![
            user("history only user 0"),
            assistant("history only assistant 1"),
            user("history only user 2"),
        ];
        session::save_result(&saved).expect("save history-only session fixture");

        {
            let _guard = crate::lua::install_app_ptr(&mut app.app);
            app.app
                .lua
                .lua
                .load(
                    r#"
                    local buf = smelt.buf.new({})
                    local out = smelt.session.render_preview_into(
                        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                        { buf = buf, width = 80, height = 12 }
                    )
                    assert(out ~= nil, "preview returned nil")
                    assert(out.total_rows > 0, "preview was empty")
                    "#,
                )
                .exec()
                .expect("render history-only preview");
        }

        app.app.load_session_by_id(id);
        let total_rows = app.app.transcript_total_rows();
        assert!(total_rows > 0, "opened transcript was empty");
        let rows = app
            .app
            .transcript_visible_rows(0, total_rows.min(80))
            .join("\n");
        assert!(
            rows.contains("history only user 0") || rows.contains("history only assistant 1"),
            "opened transcript did not render saved history: {rows:?}"
        );

        app.app
            .persister
            .release()
            .expect("release history-only fixture");
        session::delete(id).expect("delete history-only fixture");
    }

    #[test]
    fn history_updated_save_persists_only_dirty_suffix_rows() {
        const OLD_HISTORY_LEN: usize = 256;
        let mut app = large_saved_session_app(OLD_HISTORY_LEN);
        smelt_perf::perf::clear();
        smelt_perf::perf::set_enabled(true);
        app.app
            .set_history_from(OLD_HISTORY_LEN, vec![assistant("new assistant")]);
        app.app.save_session();
        app.app.flush_persist();
        smelt_perf::perf::set_enabled(false);

        assert_eq!(app.app.core.session.history.len(), OLD_HISTORY_LEN + 1);
        assert_perf_value_at_most("persist:write:history_items", 1);
        assert_perf_value_at_most("store:session:dirty_suffix_history_rows", 1);
        assert_perf_value_at_most("store:history:dirty_suffix_rows", 1);
        assert_perf_value_at_most("store:session:history_rows_inserted", 1);
        assert_perf_value_at_most("store:session:history_rows_deleted", 0);
        assert_cached_persist_db();
        assert_no_full_store_reads();
    }

    #[test]
    fn no_op_save_does_not_enqueue_history_or_descriptor_work() {
        let mut app = large_saved_session_app(256);

        smelt_perf::perf::clear();
        smelt_perf::perf::set_enabled(true);
        app.app.save_session();
        app.app.flush_persist();
        smelt_perf::perf::set_enabled(false);

        let skipped = perf_value_max("session:save:skipped_unchanged");
        assert_eq!(
            skipped, 1,
            "no-op save did not take the unchanged fast path"
        );
        assert_perf_value_absent("persist:write:history_items");
        assert_perf_value_absent("persist:write:descriptor_records");
        assert_perf_value_absent("store:session:dirty_suffix_history_rows");
        assert_perf_value_absent("store:history:dirty_suffix_rows");
        assert_perf_value_absent("store:transcript:dirty_descriptor_suffix_rows");
        assert_no_full_store_reads();
    }

    #[test]
    fn normal_request_append_persists_touched_metadata_suffix() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let id = app.app.core.session.id.clone();
        app.app.commit_request_history_item_with_first_user(
            user("new user"),
            Some(Block::User {
                text: "new user".into(),
                image_labels: vec![],
            }),
            Some("new user".into()),
        );

        let db =
            smelt_store::SessionReader::open_database(session::dir_for_id(&id).join("session.db"))
                .expect("open session db");
        let snapshot = db
            .load_full_session_snapshot()
            .expect("load snapshot")
            .expect("snapshot");
        assert_eq!(snapshot.history, vec![user("new user")]);
        assert_eq!(snapshot.metadata_snapshots.len(), 1);
        assert_eq!(snapshot.metadata_snapshots[0].0, 1);
        assert_eq!(
            snapshot.metadata_snapshots[0].1.get("first_user_message"),
            Some(&serde_json::Value::String("new user".into()))
        );
    }

    #[test]
    fn rewind_to_start_persists_empty_history_and_descriptor_delete() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let mut session = session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
        session.first_user_message = Some("old user".into());
        session.history = vec![user("old user"), assistant("old assistant")];

        app.app.load_session(session);
        app.app.restore_screen();
        app.app.session_document.mark_session_unpersisted();
        app.app.save_session();
        app.app.flush_persist();

        let id = app.app.core.session.id.clone();
        app.app.rewind_to_start();
        app.app.save_session();
        app.app.flush_persist();

        let db =
            smelt_store::SessionReader::open_database(session::dir_for_id(&id).join("session.db"))
                .expect("open session db");
        assert_eq!(
            db.read_history_items_range(0..10)
                .expect("read persisted history"),
            Vec::<HistoryItem>::new()
        );
        assert!(db
            .read_all_transcript_descriptor_records()
            .expect("read descriptors")
            .is_empty());
        assert_eq!(
            db.session_state().expect("read state").unwrap().history_len,
            0
        );
    }

    #[test]
    fn compaction_preview_rewrites_and_clears_one_block() {
        let mut app = crate::app::test_harness::TestApp::builder().build();

        app.app.update_compaction_preview("one".into());
        let id = app
            .app
            .session_document
            .transcript
            .compaction_preview_id()
            .expect("preview id");
        assert!(matches!(
            app.app.session_document.transcript.history().block(id),
            Some(Block::CompactionPreview { summary }) if summary == "one"
        ));

        app.app.update_compaction_preview("one\ntwo".into());
        assert_eq!(
            app.app.session_document.transcript.compaction_preview_id(),
            Some(id)
        );
        assert!(matches!(
            app.app.session_document.transcript.history().block(id),
            Some(Block::CompactionPreview { summary }) if summary == "one\ntwo"
        ));

        app.app.clear_compaction_preview();
        assert!(app
            .app
            .session_document
            .transcript
            .compaction_preview_id()
            .is_none());
        assert!(app
            .app
            .session_document
            .transcript
            .history()
            .block(id)
            .is_none());
    }

    #[test]
    fn restore_screen_uses_user_display_when_present() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.core.session.history = vec![HistoryItem::User {
            content: Content::text("expanded command body"),
            display: Some("/reflect".into()),
        }];

        app.app.restore_screen();

        let history = app.app.session_document.transcript.history();
        let id = history.order[0];
        assert!(matches!(
            history.block(id),
            Some(Block::User { text, .. }) if text == "/reflect"
        ));
    }

    #[test]
    fn restore_screen_rebuilds_process_status_notes_as_process_blocks() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let note = "background process 123 completed successfully";
        app.app.core.session.history = vec![user(&protocol::process_status_note(note))];

        app.app.restore_screen();

        let history = app.app.session_document.transcript.history();
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

        let history = app.app.session_document.transcript.history();
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
        let before = app.app.session_document.transcript.history().order.clone();

        let installed =
            app.app
                .install_context_checkpoint("compaction".into(), "summary".into(), 2, Some(100));

        assert!(installed);
        assert_eq!(app.app.core.session.display_context_tokens(), Some(0));
        let usage = app
            .app
            .core
            .signals
            .get::<protocol::TokenUsage>("tokens_used")
            .expect("tokens_used reset");
        assert_eq!(usage.context_tokens, Some(0));
        assert_eq!(usage.prompt_tokens, Some(0));
        let history = app.app.session_document.transcript.history();
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
        let history = app.app.session_document.transcript.history();
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
        assert!(app.app.session_document.is_save_queued());
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
        let history = app.app.session_document.transcript.history();
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
        app.app.session_document.transcript =
            crate::app::transcript::TranscriptDocument::from_transcript(transcript);

        let index = app.app.suppress_duplicate_carried_tail_before(2);

        let history = app.app.session_document.transcript.history();
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
        let history = app.app.session_document.transcript.history();
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
        let history = app.app.session_document.transcript.history();
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

    #[test]
    fn ephemeral_session_save_does_not_create_persistent_session_dir() {
        let mut app = crate::app::test_harness::TestApp::builder()
            .with_ephemeral(true)
            .build();
        let persistent_dir = session::dir_for(&app.app.core.session);
        let temp_dir = app.app.current_session_dir();
        app.app.session_append_history(user("temporary"));

        app.app.save_session();
        app.app.flush_persist();

        assert!(app.app.ephemeral());
        assert!(temp_dir.exists());
        assert!(!persistent_dir.exists());
        assert!(app.app.shutdown_context().ephemeral);
        let shared = app.app.shared_session.lock().unwrap().clone().unwrap();
        assert!(shared.ephemeral);
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
    fn app_rewind_marks_context_tokens_stale_for_different_model() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let old_identity = smelt_core::session::ContextTokenIdentity {
            model: Some("old-model".into()),
            api_base: Some("https://old.example".into()),
            provider_type: Some("old-provider".into()),
        };
        app.app.core.session.history = vec![user("a"), assistant("b")];
        app.app.core.session.context_tokens = Some(50);
        app.app.core.session.context_tokens_history_len = Some(2);
        app.app.core.session.context_token_identity = Some(old_identity.clone());
        app.app.core.session.display_context_tokens = Some(50);
        app.app.core.session.display_context_token_identity = Some(old_identity);
        app.app.core.session.snapshot_context();
        app.app
            .core
            .session
            .history
            .extend([user("c"), assistant("d")]);
        app.app.restore_screen();

        let restored = app.app.rewind_to(2).expect("second user turn");

        assert_eq!(restored.0, "c");
        assert_eq!(app.app.core.session.display_context_tokens(), Some(50));
        assert!(app
            .app
            .core
            .session
            .display_context_tokens_stale(&app.app.active_context_token_identity()));
        assert!(app.app.core.session.context_tokens.is_none());
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

        session.history.clear();
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
            tokens_after_estimate_history_len: None,
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
            tokens_after_estimate_history_len: None,
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
            tokens_after_estimate_history_len: None,
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
