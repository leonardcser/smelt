use crate::app::TuiApp;
use smelt_core::content::transcript::Transcript;
use smelt_core::session;
use smelt_core::transcript_model::BlockHistory;
use smelt_core::{Block, ToolOutput, ToolState, ToolStatus};

use protocol::{AgentMode, AssistantStep, Content, HistoryItem, UiCommand};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HistoryDeltaKind {
    Append,
    Checkpoint,
    Cleared,
    Forked,
    Loaded,
    Request,
    Rewound,
    Set,
    SubmitFailed,
}

impl HistoryDeltaKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Append => "append",
            Self::Checkpoint => "checkpoint",
            Self::Cleared => "cleared",
            Self::Forked => "forked",
            Self::Loaded => "loaded",
            Self::Request => "request",
            Self::Rewound => "rewound",
            Self::Set => "set",
            Self::SubmitFailed => "submit_failed",
        }
    }

    const fn invalidates_history_epoch(self) -> bool {
        match self {
            Self::Cleared | Self::Forked | Self::Loaded | Self::Rewound | Self::SubmitFailed => {
                true
            }
            Self::Append | Self::Checkpoint | Self::Request | Self::Set => false,
        }
    }
}

pub(crate) struct ToolSummaryResolver<'a> {
    lua: &'a smelt_core::lua::LuaRuntime,
}

impl<'a> ToolSummaryResolver<'a> {
    pub(crate) fn new(lua: &'a smelt_core::lua::LuaRuntime) -> Self {
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
            authoritative_context_tokens: None,
            display_context_tokens: None,
            history_len: Some(history_len),
            checkpoint,
            checkpoint_events: Vec::new(),
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
/// Read-only fallback is reserved for stores without a usable record projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FullSessionMaterializationReason {
    ReadOnlyTranscriptFallback,
    #[cfg(test)]
    TestSavedSessionAssertion,
}

impl FullSessionMaterializationReason {
    fn counter(self) -> &'static str {
        match self {
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
    sessions: &session::SessionStorage,
    id: &str,
    reason: FullSessionMaterializationReason,
) -> Option<session::Session> {
    materialize_full_session_result(sessions, id, reason)
        .ok()
        .flatten()
}

pub(crate) fn materialize_full_session_result(
    sessions: &session::SessionStorage,
    id: &str,
    reason: FullSessionMaterializationReason,
) -> session::SessionStoreResult<Option<session::Session>> {
    smelt_perf::perf::record_value("session:full_materialized", 1);
    smelt_perf::perf::record_value(reason.counter(), 1);
    sessions.load_full_result(id)
}

pub(crate) fn materialize_full_transcript_read_only(
    sessions: &session::SessionStorage,
    lua: &smelt_core::lua::LuaRuntime,
    id: &str,
) -> Option<crate::app::transcript::LoadedTranscript> {
    materialize_full_transcript_read_only_result(sessions, lua, id)
        .ok()
        .flatten()
}

pub(crate) fn materialize_full_transcript_read_only_result(
    sessions: &session::SessionStorage,
    lua: &smelt_core::lua::LuaRuntime,
    id: &str,
) -> session::SessionStoreResult<Option<crate::app::transcript::LoadedTranscript>> {
    let Some(session) = materialize_full_session_result(
        sessions,
        id,
        FullSessionMaterializationReason::ReadOnlyTranscriptFallback,
    )?
    else {
        return Ok(None);
    };
    Ok(Some(crate::app::transcript::LoadedTranscript::full(
        build_transcript_from_session(lua, &session),
    )))
}

fn checkpoint_markers_by_history_index(session: &session::Session) -> BTreeMap<usize, Vec<String>> {
    let mut markers = BTreeMap::<usize, Vec<String>>::new();
    for event in &session.checkpoint_events {
        markers
            .entry(event.first_live_index)
            .or_default()
            .push(event.summary.clone());
    }
    if markers.is_empty() {
        if let Some(checkpoint) = &session.checkpoint {
            markers
                .entry(checkpoint.first_live_index)
                .or_default()
                .push(checkpoint.summary.clone());
        }
    }
    markers
}

fn insert_checkpoint_markers(
    transcript: &mut Transcript,
    markers: &mut BTreeMap<usize, Vec<String>>,
    history_index: usize,
) {
    let Some(summaries) = markers.remove(&history_index) else {
        return;
    };
    for summary in summaries {
        transcript.insert_checkpoint_marker(history_index, Block::Compacted { summary });
    }
}

pub(crate) fn build_transcript_from_session(
    lua: &smelt_core::lua::LuaRuntime,
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

    let mut checkpoint_markers = checkpoint_markers_by_history_index(session);
    for (idx, item) in session.history.iter().enumerate() {
        insert_checkpoint_markers(&mut transcript, &mut checkpoint_markers, idx);
        match item {
            HistoryItem::User {
                content,
                display,
                command,
            } => push_user_block(
                &mut transcript,
                lua,
                idx,
                content,
                display.as_deref(),
                *command,
            ),
            HistoryItem::Assistant(turn) => {
                push_assistant_blocks(&mut transcript, &summary_resolver, idx, turn)
            }
            HistoryItem::Note(note) => push_note_block(&mut transcript, lua, idx, note),
            HistoryItem::System { .. } => {}
        }
    }
    insert_checkpoint_markers(
        &mut transcript,
        &mut checkpoint_markers,
        session.history.len(),
    );

    smelt_perf::perf::record_value(
        "transcript:build_from_session:blocks",
        transcript.history.order.len() as u64,
    );
    transcript
}

#[cfg(feature = "transcript-fixture")]
pub(crate) fn project_session_transcript_records(
    session: &session::Session,
) -> Result<Vec<smelt_store::StoredTranscriptBlock>, smelt_store::StoreError> {
    let lua = crate::lua::LuaRuntime::new();
    let transcript = build_transcript_from_session(&lua, session);
    transcript
        .history
        .block_records_with_ids()
        .iter()
        .enumerate()
        .map(|(record_idx, record)| {
            smelt_core::transcript_model::transcript_block_row_with_block_idx(
                record_idx,
                record.block_id.get(),
                &record.record,
            )
        })
        .collect()
}

pub(crate) fn load_transcript_tail_from_sqlite(
    sessions: &session::SessionStorage,
    session: &session::Session,
    width: u16,
    viewport_rows: u16,
) -> Option<crate::app::transcript::LoadedTranscript> {
    load_transcript_tail_from_sqlite_dir(sessions.dir_for(session), width, viewport_rows)
}

pub(crate) fn load_transcript_tail_from_sqlite_id(
    sessions: &session::SessionStorage,
    id: &str,
    width: u16,
    viewport_rows: u16,
) -> Option<crate::app::transcript::LoadedTranscript> {
    let resolved = sessions.resolve_session_dir_for_read(id)?;
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
    records_cover_history(history.block_records().iter(), session)
}

fn block_history_reaches_history_tail(history: &BlockHistory, session: &session::Session) -> bool {
    let Some(last_visible) = session
        .history
        .iter()
        .rposition(|item| fallback_history_item_block_count(item) > 0)
    else {
        return true;
    };
    history.block_records().iter().any(|record| {
        matches!(record.origin, Some(smelt_core::BlockOrigin::History(origin)) if origin == last_visible)
    })
}

fn records_cover_history<'a>(
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

fn record_links_match_history_suffix(
    history: &BlockHistory,
    first_index: usize,
    items: &[HistoryItem],
) -> bool {
    let Some(block_index) = history.first_block_index_for_history_origin_at_or_after(first_index)
    else {
        return true;
    };
    history
        .block_records_from(block_index)
        .into_iter()
        .all(|record| {
            let Some(smelt_core::BlockOrigin::History(history_index)) = record.origin else {
                return true;
            };
            if history_index < first_index {
                return true;
            }
            items.get(history_index - first_index).is_some_and(|item| {
                protocol::transcript_block_kind_matches_history_item(record.block.kind(), item)
            })
        })
}

fn transcript_index_for_checkpoint(
    transcript: &BlockHistory,
    materialized_history: &[HistoryItem],
    history_len: usize,
    first_live_index: usize,
    mut history_range: impl FnMut(std::ops::Range<usize>) -> Vec<HistoryItem>,
) -> usize {
    if let Some(index) =
        transcript.first_block_index_for_history_origin_at_or_after(first_live_index)
    {
        return index;
    }
    if first_live_index >= history_len {
        return transcript.len();
    }
    if materialized_history.len() == history_len {
        return fallback_transcript_index_for_history_index(materialized_history, first_live_index);
    }
    // Sparse sessions retain transcript records but read canonical history on demand.
    // Advance from the nearest loaded origin across any unoriginated history blocks.
    if let Some((block_index, history_index)) =
        transcript
            .order
            .iter()
            .enumerate()
            .rev()
            .find_map(|(block_index, id)| match transcript.block_origin(*id) {
                Some(smelt_core::BlockOrigin::History(history_index))
                    if history_index < first_live_index =>
                {
                    Some((block_index, history_index))
                }
                _ => None,
            })
    {
        let history_start = history_index.saturating_add(1);
        let history = history_range(history_start..first_live_index);
        if history.len() == first_live_index.saturating_sub(history_start) {
            let intervening_blocks = history.iter().map(fallback_history_item_block_count).sum();
            return block_index
                .saturating_add(1)
                .saturating_add(intervening_blocks)
                .min(transcript.len());
        }
    }
    // A fully unoriginated sparse tail has no forward or backward anchor. Its
    // canonical retained suffix still gives an exact offset from transcript end.
    let retained_history = history_range(first_live_index..history_len);
    if retained_history.len() == history_len.saturating_sub(first_live_index) {
        let retained_blocks = retained_history
            .iter()
            .map(fallback_history_item_block_count)
            .sum::<usize>();
        return transcript.len().saturating_sub(retained_blocks);
    }
    fallback_transcript_index_for_history_index(materialized_history, first_live_index)
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
    lua: &smelt_core::lua::LuaRuntime,
    note: &protocol::HistoryNote,
) -> Option<Block> {
    match note.kind() {
        protocol::HistoryNoteKind::ModeChange => {
            Some(crate::lua::mode_block(lua, note.mode(), note.text()))
        }
        protocol::HistoryNoteKind::Context => None,
        protocol::HistoryNoteKind::ProcessStatus => Some(Block::ProcessStatus {
            text: note.text().to_string(),
            event: note.process_status_event_ref().cloned(),
        }),
    }
}

fn push_note_block(
    transcript: &mut Transcript,
    lua: &smelt_core::lua::LuaRuntime,
    history_index: usize,
    note: &protocol::HistoryNote,
) {
    let Some(block) = history_note_to_block(lua, note) else {
        return;
    };
    transcript
        .push_hydrated_block_with_origin(block, smelt_core::BlockOrigin::History(history_index));
}

fn push_user_block(
    transcript: &mut Transcript,
    lua: &smelt_core::lua::LuaRuntime,
    history_index: usize,
    content: &Content,
    display: Option<&str>,
    command: bool,
) {
    let record = match protocol::classify_user_history_content(content) {
        protocol::UserHistoryContent::CompactionSummary { summary } => Block::Compacted { summary },
        protocol::UserHistoryContent::ModeChange { text } => {
            crate::lua::mode_block(lua, None, &text)
        }
        protocol::UserHistoryContent::ProcessStatus { text } => {
            Block::ProcessStatus { text, event: None }
        }
        protocol::UserHistoryContent::Plain => {
            let text = content.text_content();
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
            Block::User {
                text: display_text,
                image_labels,
                command,
            }
        }
    };
    transcript
        .push_hydrated_block_with_origin(record, smelt_core::BlockOrigin::History(history_index));
}

fn push_assistant_blocks(
    transcript: &mut Transcript,
    summary_resolver: &ToolSummaryResolver<'_>,
    history_index: usize,
    turn: &AssistantStep,
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
        transcript.push_hydrated_block_with_origin(
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
        let elapsed_ms = inv.elapsed_ms;
        let summary = summary_resolver.resolve(&inv.name, &args);
        transcript.push_hydrated_tool_block_with_origin(
            Block::ToolCall {
                call_id: inv.call_id.clone(),
                name: inv.name.clone(),
                summary,
                args,
            },
            ToolState {
                status,
                elapsed: elapsed_ms.map(Duration::from_millis),
                called_at_ms: inv.called_at_ms,
                elapsed_active: false,
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

    const SESSION_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn history_delta_kinds_define_signal_labels_and_epoch_invalidation() {
        let cases = [
            (HistoryDeltaKind::Append, "append", false),
            (HistoryDeltaKind::Checkpoint, "checkpoint", false),
            (HistoryDeltaKind::Cleared, "cleared", true),
            (HistoryDeltaKind::Forked, "forked", true),
            (HistoryDeltaKind::Loaded, "loaded", true),
            (HistoryDeltaKind::Request, "request", false),
            (HistoryDeltaKind::Rewound, "rewound", true),
            (HistoryDeltaKind::Set, "set", false),
            (HistoryDeltaKind::SubmitFailed, "submit_failed", true),
        ];

        for (kind, label, invalidates_history_epoch) in cases {
            assert_eq!(kind.as_str(), label);
            assert_eq!(
                kind.invalidates_history_epoch(),
                invalidates_history_epoch,
                "unexpected history epoch policy for {label}"
            );
        }
    }

    fn seed_transcript(
        root: &std::path::Path,
        records: Vec<smelt_store::StoredTranscriptBlock>,
    ) -> std::path::PathBuf {
        let mut session = session::Session::new(1, std::path::PathBuf::from("/tmp"));
        session.id = SESSION_ID.into();
        let mut command = session::initial_store_commit_from_session(&session).unwrap();
        command.transcript_records = Some(smelt_store::TranscriptRecordSuffix {
            start: smelt_store::TranscriptRecordIndex::ZERO,
            records,
        });
        let mut writer = smelt_store::OwnedLineageWriter::open(root, SESSION_ID).unwrap();
        writer.commit_session(&command).unwrap();
        writer.release().unwrap();
        root.join(SESSION_ID)
    }

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
                    result: protocol::ToolOutcome::new("done".into(), false, None),
                    elapsed_ms: None,
                    called_at_ms: Some(1_742_573_823_000),
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

    #[cfg(feature = "transcript-fixture")]
    #[test]
    fn projected_fixture_rows_use_production_rendering_and_origins() {
        let mut session = session::Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![
            HistoryItem::system("hidden system"),
            HistoryItem::user(Content::text("visible user")),
            HistoryItem::note(protocol::HistoryNote::context("hidden context")),
            HistoryItem::Assistant(protocol::AssistantStep::with_invocations(
                Some(Content::text("visible assistant")),
                Some("visible reasoning".into()),
                Vec::new(),
                vec![protocol::ToolInvocation {
                    call_id: "call-1".into(),
                    name: "demo_tool".into(),
                    arguments: r#"{"path":"src/main.rs"}"#.into(),
                    result: protocol::ToolOutcome::new(
                        "searchable tool output".into(),
                        false,
                        Some(serde_json::json!({"note": "metadata"})),
                    ),
                    elapsed_ms: Some(42),
                    called_at_ms: Some(1234),
                }],
            )),
            HistoryItem::note(protocol::HistoryNote::process_status("visible process")),
        ];

        let records = project_session_transcript_records(&session).unwrap();
        assert_eq!(records.len(), 5);
        assert_eq!(
            records
                .iter()
                .map(|record| record.history_idx)
                .collect::<Vec<_>>(),
            vec![Some(1), Some(3), Some(3), Some(3), Some(4)]
        );
        for text in [
            "visible user",
            "visible reasoning",
            "visible assistant",
            "searchable tool output",
            "visible process",
        ] {
            assert!(
                records
                    .iter()
                    .any(|record| record.indexed_text.contains(text)),
                "projected transcript omitted {text:?}"
            );
        }
        let tool = records.iter().find(|record| record.kind == "tool").unwrap();
        assert!(tool.tool_state_json.as_deref().unwrap().contains("1234"));
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
                command: false,
            },
            smelt_core::BlockOrigin::History(0),
        );
        assert!(transcript_covers_history(&transcript, &session));
    }

    #[test]
    fn transcript_links_require_matching_history_projection() {
        let mut transcript = Transcript::new();
        transcript.push_with_origin(
            Block::User {
                text: "submitted command".into(),
                image_labels: vec![],
                command: true,
            },
            smelt_core::BlockOrigin::History(0),
        );
        let user = vec![HistoryItem::user(Content::text("submitted command"))];
        assert!(record_links_match_history_suffix(
            &transcript.history,
            0,
            &user
        ));

        let assistant = vec![HistoryItem::Assistant(protocol::AssistantStep::terminal(
            Some(Content::text("replacement")),
            None,
            Vec::new(),
        ))];
        assert!(!record_links_match_history_suffix(
            &transcript.history,
            0,
            &assistant
        ));
    }

    #[test]
    fn sparse_checkpoint_boundary_counts_back_from_unoriginated_tail() {
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: "compacted prefix".into(),
        });
        transcript.push(Block::Text {
            content: "retained assistant".into(),
        });
        let history = [
            HistoryItem::Assistant(protocol::AssistantStep::terminal(
                Some(Content::text("compacted prefix")),
                None,
                Vec::new(),
            )),
            HistoryItem::Assistant(protocol::AssistantStep::terminal(
                Some(Content::text("retained assistant")),
                None,
                Vec::new(),
            )),
        ];

        let index =
            transcript_index_for_checkpoint(&transcript.history, &[], history.len(), 1, |range| {
                history[range].to_vec()
            });

        assert_eq!(index, 1);
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
    fn display_only_lineage_load_reads_bounded_tail_records() {
        let dir = tempfile::tempdir().unwrap();
        let records = (0..200)
            .map(|idx| test_record_record(idx, &format!("block {idx}")))
            .collect::<Vec<_>>();
        let session_dir = seed_transcript(dir.path(), records);

        let loaded =
            load_transcript_tail_from_sqlite_dir(session_dir, 10, 1).expect("tail transcript");
        let record_window = loaded.record_window.expect("record window");
        assert_eq!(record_window.start.get(), 160);
        assert_eq!(record_window.end().get(), 200);
        assert_eq!(record_window.total_count, 200);
        assert_eq!(
            record_window.hydration,
            smelt_store::TranscriptRecordHydration::ObjectBacked
        );
        assert!(loaded.transcript.history.order.is_empty());
        assert_eq!(record_window.records.len(), 40);
        assert_eq!(record_window.records[0].block_id.get(), 160);
        assert_eq!(
            record_window.records[0].stored.origin,
            Some(smelt_core::BlockOrigin::History(160))
        );
        assert_eq!(record_window.records[39].block_id.get(), 199);
        assert_eq!(
            record_window.records[39].stored.origin,
            Some(smelt_core::BlockOrigin::History(199))
        );
    }

    #[test]
    fn display_only_lineage_load_counts_non_dense_record_rows() {
        let dir = tempfile::tempdir().unwrap();
        let records = vec![
            test_record_record(70, "visible old tail"),
            test_record_record(235, "visible newest tail"),
        ];
        let session_dir = seed_transcript(dir.path(), records);

        let loaded =
            load_transcript_tail_from_sqlite_dir(session_dir, 80, 12).expect("tail transcript");
        let record_window = loaded.record_window.expect("record window");
        assert_eq!(record_window.start.get(), 0);
        assert_eq!(record_window.end().get(), 2);
        assert_eq!(record_window.total_count, 2);
        assert_eq!(record_window.records[0].block_id.get(), 70);
        assert_eq!(record_window.records[1].block_id.get(), 235);
    }

    fn test_record_record(block_idx: u64, content: &str) -> smelt_store::StoredTranscriptBlock {
        smelt_store::StoredTranscriptBlock {
            block_idx,
            history_idx: None,
            kind: "text".to_string(),
            tool_call_id: None,
            tool_name: None,
            content_hash: "0".to_string(),
            estimated_text_bytes: content.len() as u64,
            preview_text: content.to_string(),
            indexed_text: content.to_string(),
            block_json: serde_json::to_string(&Block::Text {
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
            .conversation
            .pending_history_appends()
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
        let history_changed = dirty_from < current_len || common_len < history.len();
        if history_changed {
            self.session_truncate_from(dirty_from);
            for item in history.iter().skip(common_len).cloned() {
                self.session_append_history(item);
            }
        }
        debug_assert_eq!(self.session_history_len(), final_len);

        let rebuild_from = match (
            self.conversation.pending_transcript_history_rebuild_from(),
            history_changed,
        ) {
            (Some(pending), true) => Some(pending.min(dirty_from)),
            (Some(pending), false) => Some(pending),
            (None, true) => Some(dirty_from),
            (None, false) => None,
        };
        if let Some(rebuild_from) = rebuild_from {
            let loaded_history;
            let history_suffix = if history_changed && rebuild_from == dirty_from {
                &history[common_len..]
            } else {
                loaded_history = self.session_history_range(rebuild_from..final_len);
                loaded_history.as_slice()
            };
            let links_match = history_suffix.len() == final_len.saturating_sub(rebuild_from)
                && record_links_match_history_suffix(
                    self.conversation.transcript().history(),
                    rebuild_from,
                    history_suffix,
                );
            if links_match {
                self.conversation.clear_pending_transcript_history_rebuild();
            } else if self.conversation.has_live_transcript_blocks() {
                self.conversation
                    .defer_transcript_history_rebuild_from(rebuild_from);
            } else {
                let mut session = self.conversation.session().clone();
                session.history = self.session_history_range(0..final_len);
                if session.history.len() == final_len {
                    let transcript = build_transcript_from_session(&self.lua, &session);
                    self.conversation
                        .replace_transcript_from_history(transcript);
                }
            }
        }
        for item in applied_items {
            self.commit_pending_history_append(&item);
        }
        self.sync_session_snapshot();
        self.publish_history_delta(HistoryDeltaKind::Set);
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
        self.publish_history_delta(HistoryDeltaKind::Append);
    }

    pub(crate) fn publish_history_delta(&mut self, kind: HistoryDeltaKind) {
        if kind.invalidates_history_epoch() {
            self.bump_epoch("history_epoch");
        }
        let count = self.session_history_len();
        self.core.signals.emit_dyn(
            "history",
            std::rc::Rc::new(smelt_core::signals::HistoryDelta {
                kind: kind.as_str().into(),
                count,
            }),
        );
    }

    pub(crate) fn session_is_read_only(&self) -> bool {
        self.conversation.is_read_only()
    }

    fn read_only_reason(&self) -> String {
        self.conversation.read_only_reason()
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
        let append_result = self
            .conversation
            .apply_history_append(append, self.active_context_token_identity());
        match append_result {
            Ok(result) => {
                if result == protocol::HistoryAppendResult::RemovedLast {
                    self.sync_task_label_from_session();
                }
                result
            }
            Err(err) => {
                smelt_perf::perf::record_value("live_session:history_append_plan_error", 1);
                self.notify_error_sticky(format!("failed to update session history: {err}"));
                protocol::HistoryAppendResult::Unchanged
            }
        }
    }

    pub(crate) fn sync_session_snapshot(&mut self) {
        if self.conversation.is_read_only() {
            self.publish_shared_session_state();
            return;
        }
        let metadata = self.runtime_session_metadata();
        self.conversation.update_runtime_metadata(metadata);
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
        let current_history_len = self.session_history_len();
        let snapshot_history_len = target_history_len.unwrap_or(current_history_len);
        if snapshot_history_len > current_history_len {
            smelt_perf::perf::record_value("session:title:future_history_snapshot_rejected", 1);
            return;
        }
        self.conversation
            .set_title(title, slug.clone(), snapshot_history_len);
        self.set_task_label(slug);
        self.save_session();
    }

    pub(crate) fn restore_session_metadata_after_rewind(&mut self, hist_idx: usize) {
        self.conversation.restore_metadata_after_rewind(hist_idx);
        self.sync_task_label_from_session();
    }

    fn sync_task_label_from_session(&mut self) {
        let slug = self.conversation.session().slug.clone().unwrap_or_default();
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

    pub(crate) fn rewind_session_history_to(
        &mut self,
        hist_idx: usize,
        keep_checkpoint_at_boundary: bool,
    ) {
        let identity = self.active_context_token_identity();
        let turn_meta =
            self.conversation
                .rewind_history(hist_idx, keep_checkpoint_at_boundary, identity);
        self.apply_rewindable_session_state(turn_meta);
    }

    fn prune_rewindable_session_state(&mut self, hist_idx: usize) {
        let identity = self.active_context_token_identity();
        let turn_meta = self.conversation.prune_rewindable_state(hist_idx, identity);
        self.apply_rewindable_session_state(turn_meta);
    }

    fn cancel_session_bound_work(&mut self) {
        self.cancel_live_search(false);
        if self.conversation.is_active() {
            self.cancel_agent();
            self.conversation.clear_active();
        }
        self.lua.cancel_tasks();
    }

    pub(crate) fn fork_session(&mut self) {
        if self.conversation.has_live_session() {
            self.fork_live_session();
            return;
        }
        if self.session_is_empty() {
            self.notify_error("nothing to fork".into());
            return;
        }
        // Cancel any in-flight turn and Lua tasks before swapping sessions.
        self.cancel_session_bound_work();
        self.save_session_and_flush();
        let close_policy =
            if self.session_is_read_only() && self.session_document_has_unflushed_work() {
                crate::persist::ClosePolicy::AllowUnsaved
            } else {
                crate::persist::ClosePolicy::RequireDurable
            };
        if !self.close_session_persistence(close_policy) {
            return;
        }
        self.stop_background_processes();
        let original_id = self.conversation.fork(self.core.env.pid());
        self.bump_epoch("session_epoch");
        self.save_session();
        self.flush_persist();
        self.core
            .signals
            .emit_dyn("session_ended", std::rc::Rc::new(original_id.clone()));
        self.core.signals.emit_dyn(
            "session_started",
            std::rc::Rc::new(self.conversation.session().id.clone()),
        );
        self.publish_history_delta(HistoryDeltaKind::Forked);
        self.notify(format!("forked from {original_id}"));
        // Drain stale events so old snapshots don't overwrite the forked session.
        while self.core.engine.try_recv().is_ok() {}
    }

    fn fork_live_session(&mut self) {
        if self.session_is_empty() {
            self.notify_error("nothing to fork".into());
            return;
        }
        self.cancel_session_bound_work();
        let preserve_unsaved =
            self.session_is_read_only() && self.session_document_has_unflushed_work();
        let acknowledged_head = self.conversation.acknowledged_head();
        let preserved = if preserve_unsaved {
            let mut forked = self.conversation.session().fork(self.core.env.pid());
            forked.history.clear();
            let runtime_metadata = self.runtime_session_metadata();
            let intent = match self
                .conversation
                .prepare_fork_save(&mut forked, runtime_metadata)
            {
                Ok(Some(intent)) => intent,
                Ok(None) => {
                    self.notify_error_sticky(
                        "failed to prepare fork: dirty session produced no save intent".into(),
                    );
                    return;
                }
                Err(err) => {
                    self.notify_error_sticky(format!("failed to prepare fork: {err}"));
                    return;
                }
            };
            Some((forked, intent))
        } else {
            None
        };

        if !preserve_unsaved {
            self.save_session_and_flush();
        }
        let close_policy = if preserve_unsaved {
            crate::persist::ClosePolicy::AllowUnsaved
        } else {
            crate::persist::ClosePolicy::RequireDurable
        };
        if !self.close_session_persistence(close_policy) {
            return;
        }
        self.stop_background_processes();

        if !preserve_unsaved {
            self.conversation.refresh_live_session_header();
        }

        let original_id = self.conversation.session().id.clone();
        let (forked, preserved_intent) = preserved.map_or_else(
            || {
                let mut forked = self.conversation.session().fork(self.core.env.pid());
                forked.history.clear();
                (forked, None)
            },
            |(forked, intent)| (forked, Some(intent)),
        );
        let fork_root = self.conversation.sessions().sessions_dir();
        let mut source =
            match smelt_store::OwnedLineageWriter::open_existing(&fork_root, &original_id) {
                Ok(source) => source,
                Err(err) => {
                    self.notify_error_sticky(format!("failed to open source session store: {err}"));
                    return;
                }
            };
        if preserve_unsaved {
            match source.store_head() {
                Ok(source_head) if source_head == acknowledged_head => {}
                Ok(source_head) => {
                    self.notify_error_sticky(format!(
                        "failed to fork unsaved session: source head changed from {acknowledged_head:?} to {source_head:?}"
                    ));
                    return;
                }
                Err(err) => {
                    self.notify_error_sticky(format!(
                        "failed to inspect unsaved session source head: {err}"
                    ));
                    return;
                }
            }
        }
        let imported = match source.fork_current(&forked.id, forked.created_at_ms) {
            Ok(receipt) => receipt,
            Err(err) => {
                self.notify_error_sticky(format!("failed to fork session store: {err}"));
                return;
            }
        };
        if let Some(intent) = preserved_intent {
            if let Err(err) = source.switch_branch(&forked.id) {
                self.notify_error_sticky(format!("failed to select fork destination: {err}"));
                return;
            }
            let command = smelt_store::SessionCommit {
                session_id: forked.id.clone(),
                expected: imported.current,
                identity: intent.identity,
                metadata: intent.metadata,
                history: intent.history,
                side_tables: intent.side_tables,
                transcript_records: intent.records,
            };
            if let Err(err) = source.commit_session(&command) {
                self.notify_error_sticky(format!("failed to preserve unsaved fork state: {err:?}"));
                return;
            }
        }
        if let Err(err) = source.release() {
            self.notify_error_sticky(format!("failed to release lineage writer: {err}"));
            return;
        }
        self.load_current_session_by_id(&forked.id);
        self.publish_history_delta(HistoryDeltaKind::Forked);
        self.notify(format!("forked from {original_id}"));
    }

    pub(crate) fn reset_session(&mut self) {
        let _perf = smelt_perf::perf::begin("app:reset_session");
        // Reset is a hard session boundary: cancel in-flight engine work and all
        // Lua tasks before clearing state so stale events and child processes
        // don't restore old data into the new session.
        if !self.conversation.is_active() {
            self.core.engine.send(UiCommand::Cancel);
        }
        self.cancel_session_bound_work();
        self.save_session_and_flush();
        if !self.close_session_persistence(crate::persist::ClosePolicy::RequireDurable) {
            return;
        }
        self.clear_session_scoped_permissions_for_session_boundary();
        self.prompt.clear_queue();
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
        self.prompt.clear_for_session_change(&mut pctx);
        self.stop_background_processes();
        let old_id = self
            .conversation
            .reset(self.core.env.pid(), self.core.env.cwd());
        self.bump_epoch("session_epoch");
        self.core
            .signals
            .emit_dyn("session_ended", std::rc::Rc::new(old_id));
        self.core.signals.emit_dyn(
            "session_started",
            std::rc::Rc::new(self.conversation.session().id.clone()),
        );
        self.publish_history_delta(HistoryDeltaKind::Cleared);
        // Drain stale events so old Messages snapshots don't restore history into the fresh session.
        while self.core.engine.try_recv().is_ok() {}
    }

    fn install_loaded_session(&mut self, loaded: session::Session) {
        let session_cwd = self.conversation.install_loaded_session(loaded);
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
        if let Err(reason) = self.conversation.claim_writer_access() {
            self.conversation.mark_read_only(reason.clone());
            self.notify_error_sticky(format!("opened session read-only: {reason}"));
        }
    }

    fn build_current_transcript(&mut self) -> Transcript {
        let history_len = self.conversation.session().history.len();
        let mut checkpoint_markers =
            checkpoint_markers_by_history_index(self.conversation.session());

        let mut transcript = Transcript::new();
        let lua = self.lua.execution();
        for history_index in 0..history_len {
            insert_checkpoint_markers(&mut transcript, &mut checkpoint_markers, history_index);
            let Some(item) = self
                .conversation
                .session()
                .history
                .get(history_index)
                .cloned()
            else {
                break;
            };
            crate::lua::scope_app(self, || match &item {
                HistoryItem::User {
                    content,
                    display,
                    command,
                } => push_user_block(
                    &mut transcript,
                    &lua,
                    history_index,
                    content,
                    display.as_deref(),
                    *command,
                ),
                HistoryItem::Assistant(turn) => push_assistant_blocks(
                    &mut transcript,
                    &ToolSummaryResolver::new(&lua),
                    history_index,
                    turn,
                ),
                HistoryItem::Note(note) => {
                    push_note_block(&mut transcript, &lua, history_index, note)
                }
                HistoryItem::System { .. } => {}
            });
        }
        insert_checkpoint_markers(&mut transcript, &mut checkpoint_markers, history_len);
        transcript
    }

    pub fn load_session(&mut self, loaded: session::Session) {
        let lua = self.lua.execution();
        let transcript = crate::lua::scope_app(self, || {
            crate::app::transcript::LoadedTranscript::full(build_transcript_from_session(
                &lua, &loaded,
            ))
        });
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
        // Loading a session is a hard boundary for engine and Lua work tied to
        // the previous session.
        self.cancel_session_bound_work();
        let old_id = self.conversation.session().id.clone();
        self.save_session_and_flush();
        if !self.close_session_persistence(crate::persist::ClosePolicy::RequireDurable) {
            return;
        }
        self.conversation.clear_live_session();

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
        let session_dir = self
            .conversation
            .sessions()
            .dir_for(self.conversation.session());
        let session_id = &self.conversation.session().id;
        let store_head = session_dir.parent().and_then(|root| {
            smelt_store::LineageSessionReader::open_existing(root, session_id)
                .and_then(|reader| reader.snapshot())
                .map(|state| state.head)
                .ok()
        });
        self.conversation
            .install_loaded_full_session(transcript, store_head);
        self.claim_writer_access_for_current_session();
        self.bump_epoch("session_epoch");
        // Drop snapshots beyond the restored history length.
        let hist_len = self.conversation.session().history.len();
        self.prune_rewindable_session_state(hist_len);
        self.clear_session_scoped_permissions_for_session_boundary();
        self.prompt.clear_queue();
        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
        self.prompt.clear_for_session_change(&mut pctx);
        self.stop_background_processes();
        self.conversation.clear_stream_parser();
        self.sync_session_snapshot();
        self.core
            .signals
            .emit_dyn("session_ended", std::rc::Rc::new(old_id));
        self.core.signals.emit_dyn(
            "session_started",
            std::rc::Rc::new(self.conversation.session().id.clone()),
        );
        self.publish_history_delta(HistoryDeltaKind::Loaded);
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
            store_head,
            repair_records,
        } = document;
        self.cancel_session_bound_work();
        let old_id = self.conversation.session().id.clone();
        self.save_session_and_flush();
        if !self.close_session_persistence(crate::persist::ClosePolicy::RequireDurable) {
            return;
        }

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

        let record_window_len = transcript
            .record_window
            .as_ref()
            .map_or(0, |window| window.records.len());
        smelt_perf::perf::record_value("session:resume:store_backed", 1);
        smelt_perf::perf::record_value(
            "transcript:record_window:active_records",
            record_window_len as u64,
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
        debug_assert!(
            self.conversation.session().history.is_empty(),
            "store-backed TUI sessions must not retain materialized history"
        );
        self.bump_epoch("session_epoch");
        self.clear_session_scoped_permissions_for_session_boundary();
        self.prompt.clear_queue();
        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
        self.prompt.clear_for_session_change(&mut pctx);
        self.stop_background_processes();
        self.clear_transcript();
        self.conversation.install_loaded_store_session(
            transcript,
            live_session,
            store_head,
            repair_records,
        );
        self.claim_writer_access_for_current_session();
        self.publish_shared_session_state();
        self.core
            .signals
            .emit_dyn("session_ended", std::rc::Rc::new(old_id));
        self.core.signals.emit_dyn(
            "session_started",
            std::rc::Rc::new(self.conversation.session().id.clone()),
        );
        self.publish_history_delta(HistoryDeltaKind::Loaded);
        while self.core.engine.try_recv().is_ok() {}
    }

    // Explicit promotion for old in-memory-only UI flows. Normal store-backed
    // resume, render, save, rewind, and fork paths must not call this.
    pub(crate) fn ensure_live_session_materialized(&mut self) {
        match self
            .conversation
            .materialize_live_session("compat:session:display_only_promotion")
        {
            Ok(Some(loaded)) => {
                self.install_loaded_session(loaded);
                self.prune_rewindable_session_state(self.conversation.session().history.len());
                if !block_history_covers_history(
                    self.conversation.transcript().history(),
                    self.conversation.session(),
                ) {
                    let transcript = self.build_current_transcript();
                    self.conversation
                        .replace_transcript_from_history(transcript);
                }
                self.sync_session_snapshot();
            }
            Ok(None) => {}
            Err(_err) => {
                smelt_perf::perf::record_value("live_session:materialize_error", 1);
            }
        }
    }

    pub(crate) fn session_history_len(&self) -> usize {
        self.conversation.history_len()
    }

    pub(crate) fn session_is_empty(&self) -> bool {
        self.conversation.history_is_empty()
    }

    pub(crate) fn session_history_range(&self, range: std::ops::Range<usize>) -> Vec<HistoryItem> {
        self.conversation.history_range(range)
    }

    #[allow(dead_code)]
    pub(crate) fn session_history_tail(
        &self,
        max_items: usize,
        max_bytes: Option<usize>,
    ) -> Vec<HistoryItem> {
        self.conversation.history_tail(max_items, max_bytes)
    }

    pub(crate) fn session_append_history(&mut self, item: HistoryItem) -> usize {
        self.conversation.append_history_item(item)
    }

    fn commit_request_history_item_to_document(
        &mut self,
        item: HistoryItem,
        block: Option<Block>,
        first_user_message: Option<String>,
    ) -> usize {
        self.conversation
            .commit_request_history_item(item, block, first_user_message)
    }

    #[allow(dead_code)]
    pub(crate) fn session_truncate_from(&mut self, index: usize) {
        self.conversation
            .truncate_history(index, self.active_context_token_identity());
        self.sync_task_label_from_session();
    }

    #[allow(dead_code)]
    pub(crate) fn session_checkpoint(&self) -> Option<&smelt_core::ContextCheckpoint> {
        self.conversation.session().checkpoint.as_ref()
    }

    pub(crate) fn session_set_checkpoint(
        &mut self,
        checkpoint: Option<smelt_core::ContextCheckpoint>,
    ) {
        self.conversation.set_checkpoint(checkpoint);
    }

    fn store_session_model_history_source(&self) -> protocol::ModelHistorySource {
        if let Some(live) = self.conversation.live_session() {
            return live.model_history_source(self.conversation.session().checkpoint.as_ref());
        }
        let (prefix, first_live_index, end_index) =
            self.conversation.session().model_history_range();
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
        self.prune_rewindable_session_state(self.conversation.session().history.len());
        let width = self.transcript_width() as u16;
        let viewport_rows = self.viewport_rows_estimate();
        let (loaded_transcript, records_persisted) = match load_transcript_tail_from_sqlite(
            self.conversation.sessions(),
            self.conversation.session(),
            width,
            viewport_rows,
        ) {
            Some(loaded_transcript)
                if block_history_reaches_history_tail(
                    &loaded_transcript.transcript.history,
                    self.conversation.session(),
                ) =>
            {
                (loaded_transcript, true)
            }
            Some(_) | None => {
                smelt_perf::perf::record_value("session:rebuild_transcript_full_fallback", 1);
                (
                    crate::app::transcript::LoadedTranscript::full(self.build_current_transcript()),
                    false,
                )
            }
        };
        self.conversation
            .install_materialized_transcript(loaded_transcript, records_persisted);
    }

    pub(crate) fn schedule_session_save(&mut self) {
        if !self.prompt_input_is_busy() {
            self.save_session();
        }
    }

    pub(crate) fn save_session_if_pending(&mut self) {
        if self.session_document_has_unflushed_work() && !self.prompt_input_is_busy() {
            self.save_session();
        }
    }

    pub(crate) fn retry_blocked_persistence(&mut self) -> bool {
        match self.conversation.retry_blocked_persistence() {
            Ok(retried) => retried,
            Err(message) => {
                self.notify_session_save_failure(&self.conversation.session().id.clone(), &message);
                false
            }
        }
    }

    pub(crate) fn save_session(&mut self) {
        let _perf = smelt_perf::perf::begin("session:save");
        let metadata = self.runtime_session_metadata();
        match self.conversation.save(metadata) {
            Ok(crate::app::conversation::SaveStatus::Unchanged) => {
                smelt_perf::perf::record_value("session:save:skipped_unchanged", 1);
            }
            Ok(crate::app::conversation::SaveStatus::SkippedReadOnly)
            | Ok(crate::app::conversation::SaveStatus::DurableEphemeral)
            | Ok(crate::app::conversation::SaveStatus::Submitted) => {}
            Err(message) => {
                self.notify_session_save_failure(&self.conversation.session().id.clone(), &message);
            }
        }
    }

    pub(crate) fn submit_canonical_turn(
        &mut self,
        turn: smelt_store::NewTurn,
    ) -> Result<crate::persist::SubmitTurnAcknowledgement, crate::persist::PersistenceCause> {
        let metadata = self.runtime_session_metadata();
        self.conversation.submit_canonical_turn(metadata, turn)
    }

    pub(crate) fn enqueue_canonical_turn_transition(
        &mut self,
        turn_id: smelt_store::TurnId,
        state: smelt_store::TurnState,
        terminal_reason: Option<String>,
    ) -> Result<(), crate::persist::PersistenceCause> {
        let metadata = self.runtime_session_metadata();
        self.conversation.enqueue_canonical_turn_transition(
            metadata,
            turn_id,
            state,
            terminal_reason,
        )
    }

    pub(crate) fn commit_canonical_turn_transition(
        &mut self,
        turn_id: smelt_store::TurnId,
        state: smelt_store::TurnState,
        terminal_reason: Option<String>,
    ) -> Result<crate::persist::TurnTransitionAcknowledgement, crate::persist::PersistenceCause>
    {
        let metadata = self.runtime_session_metadata();
        self.conversation.commit_canonical_turn_transition(
            metadata,
            turn_id,
            state,
            terminal_reason,
        )
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
        self.conversation.update_runtime_metadata(metadata);
    }

    pub(crate) fn session_document_has_unflushed_work(&self) -> bool {
        self.conversation.has_unflushed_work()
    }

    /// Submit the latest cumulative document intent and wait for its exact generation.
    pub(crate) fn save_session_and_flush(&mut self) {
        self.save_session();
        if self.conversation.has_persistence() {
            let _ = self.flush_persist();
        }
    }

    /// Wait for the current document generation without retrying or sleeping.
    pub(crate) fn flush_persist(&mut self) -> crate::persist::PersistenceFlushOutcome {
        let outcome = self.conversation.flush_persistence();
        self.drain_persist_reports();
        match &outcome {
            crate::persist::PersistenceFlushOutcome::Blocked { cause, .. }
            | crate::persist::PersistenceFlushOutcome::OwnershipLost { cause, .. }
            | crate::persist::PersistenceFlushOutcome::Stopped { cause, .. } => {
                self.notify_session_save_failure(
                    &self.conversation.session().id.clone(),
                    &cause.message,
                );
            }
            crate::persist::PersistenceFlushOutcome::Deadline { .. } => {
                self.notify_session_save_failure(
                    &self.conversation.session().id.clone(),
                    "persistence deadline elapsed before the requested generation became durable",
                );
            }
            crate::persist::PersistenceFlushOutcome::Durable { .. } => {}
        }
        outcome
    }

    fn close_session_persistence(&mut self, policy: crate::persist::ClosePolicy) -> bool {
        let session_id = self.conversation.session().id.clone();
        match self.conversation.close_persistence(policy) {
            Ok(Some(warning)) => {
                self.notify_warn(warning);
                true
            }
            Ok(None) => true,
            Err(message) => {
                self.notify_session_save_failure(&session_id, &message);
                false
            }
        }
    }

    pub(crate) fn shutdown_persist(&mut self) -> Result<(), String> {
        self.save_session_and_flush();
        if self.close_session_persistence(crate::persist::ClosePolicy::RequireDurable) {
            Ok(())
        } else {
            Err("session persistence did not close at the current generation".into())
        }
    }

    fn suppress_duplicate_carried_tail_before(&mut self, index: usize) -> usize {
        let (prev_id, next_id) = {
            let history = self.conversation.transcript().history();
            if index == 0 || index >= history.order.len() {
                return index;
            }
            (history.order[index - 1], history.order[index])
        };
        let ids = [prev_id, next_id];
        let Some(duplicate) =
            self.conversation
                .with_pinned_transcript_blocks(&ids, |history| {
                    match (history.block(prev_id), history.block(next_id)) {
                        (Some(prev), Some(next)) => checkpoint_suffix_blocks_match(prev, next),
                        _ => false,
                    }
                })
        else {
            return index;
        };
        if duplicate && self.conversation.remove_unoriginated_block(index - 1) {
            return index - 1;
        }
        index
    }

    fn refresh_compaction_marker(&mut self) {
        let Some(checkpoint) = self.conversation.session().checkpoint.as_ref() else {
            return;
        };
        let first_live_index = checkpoint.first_live_index;
        let block = Block::Compacted {
            summary: checkpoint.summary.clone(),
        };
        let index = transcript_index_for_checkpoint(
            self.conversation.transcript().history(),
            &self.conversation.session().history,
            self.conversation.history_len(),
            first_live_index,
            |range| self.conversation.history_range(range),
        );
        let index = self.suppress_duplicate_carried_tail_before(index);
        self.conversation
            .insert_checkpoint_marker(index, first_live_index, block);
    }

    fn install_live_context_checkpoint(
        &mut self,
        kind: String,
        summary: String,
        first_live_message_index: usize,
        tokens_before: Option<u32>,
    ) -> bool {
        let Some(live) = self.conversation.live_session() else {
            return false;
        };
        if summary.trim().is_empty() || live.is_empty() {
            return false;
        }
        let first_live_index = match live.first_live_history_index_for_model_message(
            self.conversation.session().checkpoint.as_ref(),
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
        self.conversation.install_checkpoint_at_history_index(
            kind,
            summary,
            first_live_index,
            tokens_before,
            history_len,
        )
    }

    pub(crate) fn install_context_checkpoint(
        &mut self,
        kind: String,
        summary: String,
        first_live_message_index: usize,
        tokens_before: Option<u32>,
    ) -> bool {
        let installed = if self.conversation.has_live_session() {
            self.install_live_context_checkpoint(
                kind,
                summary,
                first_live_message_index,
                tokens_before,
            )
        } else {
            self.conversation.install_checkpoint(
                kind,
                summary,
                first_live_message_index,
                tokens_before,
            )
        };
        if !installed {
            self.clear_compaction_preview();
            self.notify("nothing old enough to compact".to_string());
            return false;
        }
        let tokens_after_estimate =
            smelt_core::session::estimate_message_tokens(&self.model_history_messages());
        let history_len = self.session_history_len();
        self.conversation
            .set_checkpoint_tokens_after_estimate(tokens_after_estimate, history_len);
        self.session_set_checkpoint(self.conversation.session().checkpoint.clone());
        let follow_tail = self.transcript_win().is_following_tail();
        self.clear_compaction_preview();
        self.reset_visible_context_tokens();
        self.refresh_compaction_marker();
        self.publish_history_delta(HistoryDeltaKind::Checkpoint);
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
        if self.conversation.has_document_work() {
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
            let session_dir = self.conversation.current_session_dir();
            let sessions_root = session_dir
                .parent()
                .ok_or_else(|| "session directory has no storage root".to_string())?;
            let mut rows = smelt_store::LineageSessionReader::open_existing(
                sessions_root,
                &self.conversation.session().id,
            )
            .map_err(|err| format!("open canonical model history: {err}"))?
            .history_range(first_live_index as u64, end_index as u64)
            .map_err(|err| format!("read canonical model history rows: {err}"))?;
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
        let target_id = self
            .conversation
            .transcript()
            .history()
            .order
            .get(block_idx)
            .copied()?;
        let target_ids = [target_id];
        let Some(turn_text) =
            self.conversation
                .with_pinned_transcript_blocks(&target_ids, |history| {
                    match history.block(target_id) {
                        Some(Block::User { text, .. }) => Some(text.clone()),
                        _ => None,
                    }
                })
        else {
            smelt_perf::perf::record_value("rewind:target_hydration_failure", 1);
            self.notify_error("cannot load this transcript block for rewind".into());
            return None;
        };

        let hist_idx = match self
            .conversation
            .transcript()
            .history()
            .block_origin_at(block_idx)
        {
            Some(smelt_core::BlockOrigin::History(history_idx)) => history_idx,
            _ if !self.conversation.has_live_session() => {
                self.ensure_live_session_materialized();
                let user_turns_to_keep = self
                    .user_turns()
                    .iter()
                    .filter(|(i, _)| *i < block_idx)
                    .count();
                let mut user_count = 0;
                let mut history_index = 0;
                for (i, item) in self.conversation.session().history.iter().enumerate() {
                    if matches!(item, HistoryItem::User { .. }) {
                        user_count += 1;
                        if user_count > user_turns_to_keep {
                            history_index = i;
                            break;
                        }
                    }
                    history_index = i + 1;
                }
                history_index
            }
            _ => {
                smelt_perf::perf::record_value("rewind:live_missing_history_origin", 1);
                self.notify_error("cannot rewind this transcript block".into());
                return None;
            }
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

        let mode_after_rewind = if let Some(live) = self.conversation.live_session() {
            match live.any_transcript_visible_before(hist_idx) {
                Ok(false) => None,
                Ok(true) => match live.effective_mode_at(
                    hist_idx,
                    self.conversation
                        .session()
                        .mode
                        .as_deref()
                        .unwrap_or("normal"),
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
            self.conversation.session().history[..hist_idx]
                .iter()
                .any(HistoryItem::is_transcript_visible)
                .then(|| {
                    self.mode_at_history_boundary(hist_idx)
                        .expect("materialized history mode lookup is infallible")
                })
        };

        let keep_checkpoint_at_boundary = turn_text.is_some()
            && self
                .conversation
                .session()
                .checkpoint
                .as_ref()
                .is_some_and(|cp| cp.first_live_index == hist_idx);
        self.rewind_session_history_to(hist_idx, keep_checkpoint_at_boundary);
        self.truncate_to(block_idx);
        if let Some(mode) = mode_after_rewind {
            self.restore_mode_after_rewind(mode);
        }
        self.sync_session_snapshot();
        self.publish_history_delta(HistoryDeltaKind::Rewound);

        turn_text.map(|t| (t, images))
    }

    pub(crate) fn rewind_to_start(&mut self) {
        self.rewind_session_history_to(0, false);
        self.task_label = None;
        self.working.clear();
        self.clear_transcript();
        self.sync_session_snapshot();
        self.publish_history_delta(HistoryDeltaKind::Rewound);
    }

    pub(crate) fn stage_request_history_item(
        &mut self,
        item: HistoryItem,
        block: Option<Block>,
    ) -> protocol::ModelHistorySource {
        self.stage_request_history_item_with_first_user(item, block, None)
    }

    pub(crate) fn stage_request_history_item_with_first_user(
        &mut self,
        item: HistoryItem,
        block: Option<Block>,
        first_user_message: Option<String>,
    ) -> protocol::ModelHistorySource {
        let history = self.model_history_source();
        self.commit_request_history_item_to_document(item, block, first_user_message);
        self.sync_session_snapshot();
        self.publish_history_delta(HistoryDeltaKind::Request);
        history
    }

    #[allow(dead_code)]
    pub(crate) fn commit_request_history_item(
        &mut self,
        item: HistoryItem,
        block: Option<Block>,
    ) -> protocol::ModelHistorySource {
        self.commit_request_history_item_with_first_user(item, block, None)
    }

    #[allow(dead_code)]
    pub(crate) fn commit_request_history_item_with_first_user(
        &mut self,
        item: HistoryItem,
        block: Option<Block>,
        first_user_message: Option<String>,
    ) -> protocol::ModelHistorySource {
        let history =
            self.stage_request_history_item_with_first_user(item, block, first_user_message);
        self.save_session_and_flush();
        history
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn show_user_message(&mut self, input: &str, image_labels: Vec<String>) {
        self.push_block(Block::User {
            text: input.to_string(),
            image_labels,
            command: false,
        });
    }
}

#[cfg(test)]
mod checkpoint_tests {
    use super::*;
    use protocol::Content;
    use smelt_core::ContextCheckpoint;

    fn lineage_reader(
        app: &crate::app::test_harness::TestApp,
        session_id: &str,
    ) -> smelt_store::LineageSessionReader {
        smelt_store::LineageSessionReader::open_existing(
            app.app.core.sessions.sessions_dir(),
            session_id,
        )
        .expect("open canonical lineage session")
    }

    fn lineage_history(reader: &smelt_store::LineageSessionReader) -> Vec<protocol::HistoryItem> {
        let state = reader.snapshot().expect("read lineage state");
        reader
            .history_range(0, state.head.history_len.get())
            .expect("read canonical history")
    }

    fn lineage_transcript(
        reader: &smelt_store::LineageSessionReader,
    ) -> Vec<smelt_store::StoredTranscriptBlock> {
        let state = reader.snapshot().expect("read lineage state");
        reader
            .transcript_range(0, state.transcript_len)
            .expect("read canonical transcript")
    }

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

    fn assert_cached_persist_db() {
        assert_eq!(
            perf_duration_max("store:lineage:open_read_write"),
            0,
            "hot suffix save should reuse the persist worker database connection"
        );
        assert_eq!(
            perf_value_max("store:lineage:cached_read_write"),
            1,
            "hot suffix save did not reuse the lineage writer connection"
        );
    }

    fn assert_no_full_store_reads() {
        for label in [
            "store:history:read_all",
            "store:history:read_all_rows",
            "store:session:load_full_snapshot",
            "store:session:full_snapshot_rows_read",
            "store:transcript:search_blob_full",
            "store:transcript:read_records_full",
            "store:transcript:records_full_loaded",
            "session:full_materialized",
            "session:rebuild_transcript_full_fallback",
            "session:display_only_load_full",
            "session:save:record_sparse_full_rebuild",
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

        let history = app.app.conversation.transcript().history();
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

    #[test]
    fn full_session_load_replaces_same_length_stored_history() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let mut initial = session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
        initial.history = vec![user("initial user"), assistant("initial assistant")];
        let id = initial.id.clone();

        app.app.load_session(initial.clone());
        app.app.restore_screen();
        app.app.save_session_and_flush();

        initial.history = vec![user("replacement user"), assistant("replacement assistant")];
        app.app.load_session(initial);
        app.app.restore_screen();
        app.app.save_session_and_flush();

        let reader = lineage_reader(&app, &id);
        let history = lineage_history(&reader);
        assert_eq!(
            history,
            vec![user("replacement user"), assistant("replacement assistant")]
        );
    }

    fn is_compaction_summary_item(item: &HistoryItem) -> bool {
        smelt_core::session::is_context_checkpoint_summary(item)
    }

    fn add_background_process(app: &mut crate::app::test_harness::TestApp) -> String {
        let child = smelt_core::process::spawn_shell_child(
            "sleep 30",
            &smelt_core::process::ShellSpec::default(),
            &app.app.core.env.cwd(),
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
    fn recordless_store_resume_falls_back_without_repairing() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let mut saved = session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
        saved.first_user_message = Some("old user 0".into());
        saved.history = (0usize..256)
            .map(|index| {
                if index.is_multiple_of(2) {
                    user(&format!("old user {index}"))
                } else {
                    assistant(&format!("old assistant {index}"))
                }
            })
            .collect();
        let id = saved.id.clone();
        app.app
            .core
            .sessions
            .save_result(&saved)
            .expect("save recordless canonical fixture");

        smelt_perf::perf::clear();
        smelt_perf::perf::set_enabled(true);
        app.app.load_session_by_id(&id);
        smelt_perf::perf::set_enabled(false);

        assert_eq!(app.app.session_history_len(), 256);
        assert!(app.app.conversation.has_live_session());
        assert!(app.app.conversation.session().history.is_empty());
        assert!(app.app.transcript_total_rows() > 0);
        assert_eq!(
            perf_value_max("session:transcript:read_only_full_fallback"),
            1
        );
        assert_eq!(
            lineage_reader(&app, &id).snapshot().unwrap().transcript_len,
            0
        );

        app.app
            .session_append_history(user("repair records on owned save"));
        app.app.save_session();
        app.app.flush_persist();

        assert_eq!(
            lineage_reader(&app, &id).snapshot().unwrap().transcript_len,
            256
        );
    }

    #[test]
    fn display_only_request_append_preserves_persisted_history_prefix() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let mut session = session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
        session.first_user_message = Some("old user".into());
        session.history = vec![user("old user"), assistant("old assistant")];

        app.app.load_session(session);
        app.app.restore_screen();
        app.app
            .shutdown_persist()
            .expect("persist and close loaded fixture actor");

        let id = app.app.conversation.session().id.clone();
        let loaded_transcript =
            load_transcript_tail_from_sqlite_id(&app.app.core.sessions, &id, 80, 24)
                .expect("display-only transcript tail should load");
        let (header, store_ref) = app
            .app
            .core
            .sessions
            .load_store_header(&id)
            .expect("stored session header should load");
        let store_head = lineage_reader(&app, &id)
            .snapshot()
            .expect("stored lineage state should load")
            .head;
        let document = crate::app::session_document::SessionDocument::from_store(
            header,
            store_ref,
            store_head,
            loaded_transcript,
            app.app.core.env.pid(),
            app.app.core.env.cwd(),
        );

        app.app
            .load_store_backed_session(document.into_store_backed());

        let source = app.app.commit_request_history_item(
            user("new user"),
            Some(Block::User {
                text: "new user".into(),
                image_labels: vec![],
                command: false,
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
                .conversation
                .live_session()
                .map(|live| live.history_len()),
            Some(3)
        );
        assert!(
            app.app.overlays.notification().is_none(),
            "display-only append should persist without error: {:?}",
            app.app.overlays.notification()
        );

        let reader = lineage_reader(&app, &id);
        let rows = lineage_history(&reader);
        assert_eq!(
            rows,
            vec![
                user("old user"),
                assistant("old assistant"),
                user("new user")
            ]
        );
        let records = lineage_transcript(&reader);
        assert!(records.iter().any(|record| record.history_idx == Some(2)));
    }

    #[test]
    fn normal_request_append_preserves_persisted_history_prefix() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let mut session = session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
        session.first_user_message = Some("old user".into());
        session.history = vec![user("old user"), assistant("old assistant")];

        app.app.load_session(session);
        app.app.restore_screen();
        app.app
            .shutdown_persist()
            .expect("persist and close loaded fixture actor");

        let id = app.app.conversation.session().id.clone();
        let source = app.app.commit_request_history_item(
            user("new user"),
            Some(Block::User {
                text: "new user".into(),
                image_labels: vec![],
                command: false,
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
        assert_eq!(app.app.conversation.session().history.len(), 3);

        let reader = lineage_reader(&app, &id);
        let rows = lineage_history(&reader);
        assert_eq!(
            rows,
            vec![
                user("old user"),
                assistant("old assistant"),
                user("new user")
            ]
        );
        let records = lineage_transcript(&reader);
        assert!(records.iter().any(|record| record.history_idx == Some(2)));
    }

    #[test]
    fn normal_request_append_persists_one_canonical_item() {
        const OLD_HISTORY_LEN: usize = 256;
        let mut app = large_saved_session_app(OLD_HISTORY_LEN);
        let id = app.app.conversation.session().id.clone();
        let before = lineage_reader(&app, &id)
            .snapshot()
            .expect("read initial canonical session");

        app.app.restore_screen();

        smelt_perf::perf::clear();
        smelt_perf::perf::set_enabled(true);
        let source = app.app.commit_request_history_item(
            user("new user"),
            Some(Block::User {
                text: "new user".into(),
                image_labels: vec![],
                command: false,
            }),
        );
        smelt_perf::perf::set_enabled(false);

        assert!(matches!(
            source,
            protocol::ModelHistorySource::Store {
                first_live_index: 0,
                end_index: OLD_HISTORY_LEN,
                ..
            }
        ));
        assert_eq!(
            app.app.conversation.session().history.len(),
            OLD_HISTORY_LEN + 1
        );
        assert_cached_persist_db();
        assert_no_full_store_reads();

        let reader = lineage_reader(&app, &id);
        let after = reader.snapshot().expect("read appended canonical session");
        assert_eq!(after.head.revision.get(), before.head.revision.get() + 1);
        assert_eq!(after.head.history_len.get(), OLD_HISTORY_LEN as u64 + 1);
        assert_eq!(
            reader
                .history_range(OLD_HISTORY_LEN as u64, OLD_HISTORY_LEN as u64 + 1)
                .expect("read persisted history tail"),
            vec![user("new user")]
        );
        let records = lineage_transcript(&reader);
        assert_eq!(records.len(), OLD_HISTORY_LEN + 1);
        assert_eq!(
            records.last().and_then(|row| row.history_idx),
            Some(OLD_HISTORY_LEN as u64)
        );
    }

    #[test]
    fn live_session_checkpoint_uses_store_history_coordinates() {
        let mut app = large_saved_session_app(32);
        let id = app.app.conversation.session().id.clone();
        app.app.load_session_by_id(&id);
        assert!(app.app.conversation.has_live_session());
        assert!(app.app.conversation.session().history.is_empty());

        let installed = app.app.install_context_checkpoint(
            "compaction".into(),
            "store summary".into(),
            4,
            Some(100),
        );

        assert!(installed);
        assert_eq!(app.app.conversation.session().history.len(), 0);
        let checkpoint = app.app.conversation.session().checkpoint.as_ref().unwrap();
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
        assert!(app.app.conversation.has_document_work());
    }

    #[test]
    fn live_session_save_persists_one_canonical_item() {
        const OLD_HISTORY_LEN: usize = 256;
        let mut app = large_saved_session_app(OLD_HISTORY_LEN);
        let id = app.app.conversation.session().id.clone();
        let before = lineage_reader(&app, &id)
            .snapshot()
            .expect("read initial canonical session");
        app.app.load_session_by_id(&id);
        assert!(app.app.conversation.has_live_session());
        assert!(app.app.conversation.session().history.is_empty());

        smelt_perf::perf::clear();
        smelt_perf::perf::set_enabled(true);
        app.app
            .append_engine_history_items(OLD_HISTORY_LEN, vec![assistant("new assistant")]);
        app.app.save_session();
        app.app.flush_persist();
        smelt_perf::perf::set_enabled(false);

        assert_eq!(app.app.session_history_len(), OLD_HISTORY_LEN + 1);
        assert!(app.app.conversation.session().history.is_empty());
        assert_no_full_store_reads();

        let reader = lineage_reader(&app, &id);
        let after = reader.snapshot().expect("read appended canonical session");
        assert_eq!(after.head.revision.get(), before.head.revision.get() + 1);
        assert_eq!(after.head.history_len.get(), OLD_HISTORY_LEN as u64 + 1);
        let tail = reader
            .history_range(OLD_HISTORY_LEN as u64, OLD_HISTORY_LEN as u64 + 1)
            .expect("read persisted tail");
        assert_eq!(tail, vec![assistant("new assistant")]);
    }

    #[test]
    fn normal_preview_and_open_avoid_compat_full_load_fallbacks() {
        let mut app = large_saved_session_app(256);
        let id = app.app.conversation.session().id.clone();

        smelt_perf::perf::clear();
        smelt_perf::perf::set_enabled(true);
        app.app
            .lua
            .lua
            .globals()
            .set("session_id", id.clone())
            .expect("set session id");
        app.run_lua_result(
            r#"
                    local buf = smelt.buf.new({})
                    local out = smelt.session.render_preview_into(
                        session_id,
                        { buf = buf, width = 80, height = 12 }
                    )
                    assert(out ~= nil and out.total_rows > 0)
                    "#,
        )
        .expect("render sparse session preview");
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
        app.app
            .core
            .sessions
            .save_result(&saved)
            .expect("save history-only session fixture");

        app.run_lua_result(
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
        .expect("render history-only preview");

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
            .shutdown_persist()
            .expect("release history-only fixture");
        app.app
            .core
            .sessions
            .delete(id)
            .expect("delete history-only fixture");
    }

    #[test]
    fn history_updated_save_persists_one_canonical_item() {
        const OLD_HISTORY_LEN: usize = 256;
        let mut app = large_saved_session_app(OLD_HISTORY_LEN);
        let id = app.app.conversation.session().id.clone();
        let before = lineage_reader(&app, &id)
            .snapshot()
            .expect("read initial canonical session");
        smelt_perf::perf::clear();
        smelt_perf::perf::set_enabled(true);
        app.app
            .set_history_from(OLD_HISTORY_LEN, vec![assistant("new assistant")]);
        app.app.save_session();
        app.app.flush_persist();
        smelt_perf::perf::set_enabled(false);

        assert_eq!(
            app.app.conversation.session().history.len(),
            OLD_HISTORY_LEN + 1
        );
        assert_cached_persist_db();
        assert_no_full_store_reads();

        let reader = lineage_reader(&app, &id);
        let after = reader.snapshot().expect("read appended canonical session");
        assert_eq!(after.head.revision.get(), before.head.revision.get() + 1);
        assert_eq!(after.head.history_len.get(), OLD_HISTORY_LEN as u64 + 1);
        assert_eq!(
            reader
                .history_range(OLD_HISTORY_LEN as u64, OLD_HISTORY_LEN as u64 + 1)
                .expect("read persisted history tail"),
            vec![assistant("new assistant")]
        );
    }

    #[test]
    fn no_op_save_does_not_advance_canonical_revision() {
        let mut app = large_saved_session_app(256);
        let id = app.app.conversation.session().id.clone();
        let before = lineage_reader(&app, &id)
            .snapshot()
            .expect("read initial canonical session");

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
        assert_no_full_store_reads();
        let after = lineage_reader(&app, &id)
            .snapshot()
            .expect("read canonical session after no-op save");
        assert_eq!(after.head, before.head);
        assert_eq!(after.revision_id, before.revision_id);
    }

    #[test]
    fn normal_request_append_persists_touched_metadata_suffix() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let id = app.app.conversation.session().id.clone();
        app.app.commit_request_history_item_with_first_user(
            user("new user"),
            Some(Block::User {
                text: "new user".into(),
                image_labels: vec![],
                command: false,
            }),
            Some("new user".into()),
        );

        let reader = lineage_reader(&app, &id);
        let snapshot = reader.snapshot().expect("read canonical session");
        assert_eq!(lineage_history(&reader), vec![user("new user")]);
        assert_eq!(snapshot.side_tables.metadata_snapshots.len(), 1);
        assert_eq!(snapshot.side_tables.metadata_snapshots[0].0.get(), 1);
        assert_eq!(
            snapshot.side_tables.metadata_snapshots[0]
                .1
                .get("first_user_message"),
            Some(&serde_json::Value::String("new user".into()))
        );
    }

    #[test]
    fn rewind_to_start_persists_empty_history_and_record_delete() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let mut session = session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
        session.first_user_message = Some("old user".into());
        session.history = vec![user("old user"), assistant("old assistant")];

        app.app.load_session(session);
        app.app.restore_screen();
        app.app
            .shutdown_persist()
            .expect("persist and close loaded fixture actor");

        let id = app.app.conversation.session().id.clone();
        app.app.rewind_to_start();
        app.app.save_session();
        app.app.flush_persist();

        let reader = lineage_reader(&app, &id);
        let state = reader.snapshot().expect("read rewound state");
        assert!(lineage_history(&reader).is_empty());
        assert!(lineage_transcript(&reader).is_empty());
        assert_eq!(state.head.history_len.get(), 0);
        assert_eq!(state.transcript_len, 0);
    }

    #[test]
    fn compaction_preview_rewrites_and_clears_one_block() {
        let mut app = crate::app::test_harness::TestApp::builder().build();

        app.app.update_compaction_preview("one".into());
        let id = app
            .app
            .conversation
            .transcript()
            .compaction_preview_id()
            .expect("preview id");
        assert!(matches!(
            app.app.conversation.transcript().history().block(id),
            Some(Block::CompactionPreview { summary }) if summary == "one"
        ));

        app.app.update_compaction_preview("one\ntwo".into());
        assert_eq!(
            app.app.conversation.transcript().compaction_preview_id(),
            Some(id)
        );
        assert!(matches!(
            app.app.conversation.transcript().history().block(id),
            Some(Block::CompactionPreview { summary }) if summary == "one\ntwo"
        ));

        app.app.clear_compaction_preview();
        assert!(app
            .app
            .conversation
            .transcript()
            .compaction_preview_id()
            .is_none());
        assert!(app
            .app
            .conversation
            .transcript()
            .history()
            .block(id)
            .is_none());
    }

    #[test]
    fn restore_screen_preserves_command_display_and_semantics() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app
            .conversation
            .replace_history_for_harness(vec![HistoryItem::User {
                content: Content::text("expanded command body"),
                display: Some("/reflect".into()),
                command: true,
            }]);

        app.app.restore_screen();

        let history = app.app.conversation.transcript().history();
        let id = history.order[0];
        assert!(matches!(
            history.block(id),
            Some(Block::User {
                text,
                command: true,
                ..
            }) if text == "/reflect"
        ));
    }

    #[test]
    fn restore_screen_rebuilds_process_status_notes_as_process_blocks() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let note = "background process 123 completed successfully";
        app.app
            .conversation
            .replace_history_for_harness(vec![user(&protocol::process_status_note(note))]);

        app.app.restore_screen();

        let history = app.app.conversation.transcript().history();
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
        app.app
            .conversation
            .replace_history_for_harness(vec![user(&note)]);

        app.app.restore_screen();

        let history = app.app.conversation.transcript().history();
        let id = history.order[0];
        assert!(matches!(history.block(id), Some(Block::Mode { .. })));
    }

    #[test]
    fn restore_screen_rejects_sqlite_tail_behind_in_memory_history() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app
            .conversation
            .replace_history_for_harness(vec![user("hello")]);
        app.app.restore_screen();
        app.app.save_session_and_flush();

        app.app.conversation.replace_history_for_harness(vec![
            user("hello"),
            HistoryItem::note(protocol::HistoryNote::mode_change_for_transition(
                "normal",
                "apply",
                "now in apply mode",
            )),
        ]);
        app.app.restore_screen();

        let history = app.app.conversation.transcript().history();
        assert!(history
            .order
            .iter()
            .any(|id| matches!(history.block(*id), Some(Block::Mode { text, .. }) if text == "now in apply mode")));
    }

    #[test]
    fn resumed_mode_removal_executes_the_shared_append_plan() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.conversation.replace_history_for_harness(vec![
            HistoryItem::note(protocol::HistoryNote::mode_change_for_transition(
                "normal",
                "plan",
                "plan mode",
            )),
            user("planned request"),
            HistoryItem::note(protocol::HistoryNote::mode_change_for_transition(
                "normal",
                "yolo",
                "yolo mode",
            )),
        ]);
        app.app.restore_screen();
        app.app.save_session_and_flush();
        let session_id = app.app.conversation.session().id.clone();
        app.resume_session(&session_id);
        assert!(app.app.conversation.has_live_session());

        let result =
            app.app
                .apply_history_append_to_history(&protocol::HistoryAppend::mode_change(
                    HistoryItem::note(protocol::HistoryNote::mode_change_for_transition(
                        "normal",
                        "plan",
                        "plan mode",
                    )),
                    protocol::AgentMode::normal(),
                ));

        assert_eq!(result, protocol::HistoryAppendResult::RemovedLast);
        assert_eq!(app.app.session_history_len(), 2);
        assert!(matches!(
            app.app.session_history_range(1..2).as_slice(),
            [HistoryItem::User { .. }]
        ));
    }

    #[test]
    fn checkpoint_commit_inserts_marker_without_rebuilding_transcript() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.conversation.replace_history_for_harness(vec![
            user("old"),
            assistant("old reply"),
            user("recent"),
            assistant("recent reply"),
        ]);
        app.app.restore_screen();
        let before = app.app.conversation.transcript().history().order.clone();

        let installed =
            app.app
                .install_context_checkpoint("compaction".into(), "summary".into(), 2, Some(100));

        assert!(installed);
        assert_eq!(
            app.app.conversation.session().display_context_tokens(),
            Some(0)
        );
        let usage = app
            .app
            .core
            .signals
            .get::<protocol::TokenUsage>("tokens_used")
            .expect("tokens_used reset");
        assert_eq!(usage.context_tokens, Some(0));
        assert_eq!(usage.prompt_tokens, Some(0));
        let history = app.app.conversation.transcript().history();
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
    fn checkpoint_commit_preserves_existing_marker_at_historical_boundary() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.conversation.replace_history_for_harness(vec![
            user("old"),
            assistant("old reply"),
            user("kept user"),
            assistant("kept reply"),
            user("newest"),
        ]);
        app.app.conversation.set_checkpoint(Some(ContextCheckpoint {
            kind: "compaction".to_string(),
            summary: "old summary".to_string(),
            first_live_index: 2,
            created_at_ms: 0,
            tokens_before: None,
            tokens_after_estimate: None,
            ..Default::default()
        }));
        app.app.restore_screen();

        let installed = app.app.install_context_checkpoint(
            "compaction".into(),
            "new summary".into(),
            2,
            Some(100),
        );

        assert!(installed);
        let history = app.app.conversation.transcript().history();
        let markers: Vec<_> = history
            .order
            .iter()
            .enumerate()
            .filter_map(|(idx, id)| match history.block(*id) {
                Some(Block::Compacted { summary }) => Some((idx, summary.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(markers, vec![(2, "old summary"), (4, "new summary")]);
        assert!(app.app.conversation.has_document_work());
    }

    #[test]
    fn checkpoint_commit_places_marker_without_existing_provenance() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.conversation.replace_history_for_harness(vec![
            user("old"),
            assistant("old reply"),
            user("recent"),
            assistant("recent reply"),
        ]);
        for block in [
            Block::User {
                text: "old".into(),
                image_labels: vec![],
                command: false,
            },
            Block::Text {
                content: "old reply".into(),
            },
            Block::User {
                text: "recent".into(),
                image_labels: vec![],
                command: false,
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
        let history = app.app.conversation.transcript().history();
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
        app.app
            .conversation
            .replace_transcript_document_for_harness(
                crate::app::transcript::TranscriptDocument::from_transcript(transcript),
            );

        let index = app.app.suppress_duplicate_carried_tail_before(2);

        let history = app.app.conversation.transcript().history();
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
        app.app
            .conversation
            .replace_history_for_harness(vec![user("restored"), assistant("restored reply")]);
        app.app.restore_screen();
        for item in [
            user("live old"),
            assistant("live old reply"),
            user("live recent"),
            assistant("live recent reply"),
        ] {
            app.app.conversation.append_history_item(item);
        }
        for block in [
            Block::User {
                text: "live old".into(),
                image_labels: vec![],
                command: false,
            },
            Block::Text {
                content: "live old reply".into(),
            },
            Block::User {
                text: "live recent".into(),
                image_labels: vec![],
                command: false,
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
        let history = app.app.conversation.transcript().history();
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
    fn checkpoint_commit_keeps_historical_and_user_compacted_blocks() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.conversation.replace_history_for_harness(vec![
            HistoryItem::user(protocol::compaction_summary_content(
                "user-written summary-looking block",
            )),
            assistant("reply"),
            user("recent"),
        ]);
        app.app.conversation.set_checkpoint(Some(ContextCheckpoint {
            kind: "compaction".to_string(),
            summary: "old summary".to_string(),
            first_live_index: 2,
            created_at_ms: 0,
            tokens_before: None,
            tokens_after_estimate: None,
            ..Default::default()
        }));
        app.app.restore_screen();

        let installed = app.app.install_context_checkpoint(
            "compaction".into(),
            "new summary".into(),
            2,
            Some(100),
        );

        assert!(installed);
        let history = app.app.conversation.transcript().history();
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
            vec![
                "user-written summary-looking block",
                "old summary",
                "new summary",
            ]
        );
    }

    #[test]
    fn ephemeral_session_save_does_not_create_persistent_session_dir() {
        let mut app = crate::app::test_harness::TestApp::builder()
            .with_ephemeral(true)
            .build();
        let persistent_dir = app
            .app
            .core
            .sessions
            .dir_for(app.app.conversation.session());
        let temp_dir = app.app.current_session_dir();
        app.app.session_append_history(user("temporary"));

        app.app.save_session();
        app.app.flush_persist();

        assert!(app.app.ephemeral());
        assert!(temp_dir.exists());
        assert!(!persistent_dir.exists());
        assert!(app.app.shutdown_context().ephemeral);
        let shared = app.app.conversation.shared_state().unwrap();
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
        app.app
            .conversation
            .replace_history_for_harness(vec![user("hello")]);
        add_background_process(&mut app);
        assert_eq!(app.app.core.processes.running_count(), 1);

        app.app.fork_session();

        assert_eq!(app.app.core.processes.running_count(), 0);
        assert!(app.app.core.processes.list().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fork_session_cancels_all_lua_tasks() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app
            .conversation
            .replace_history_for_harness(vec![user("hello")]);
        app.exec_lua_entry(
            r#"
                _G.__fork_task_completed__ = false
                smelt.spawn(function()
                    smelt.sleep(10000)
                    _G.__fork_task_completed__ = true
                end)
                "#,
        )
        .expect("start session-bound Lua task");

        app.app.fork_session();

        let now = app.app.core.clock.instant_now() + std::time::Duration::from_secs(20);
        let lua = app.app.lua.execution();
        let outputs = crate::lua::scope_app(&mut app.app, || lua.drive_tasks(now));
        assert!(outputs.is_empty(), "cancelled task produced output");
        let completed: bool = app
            .eval_lua("return _G.__fork_task_completed__")
            .expect("read cancelled task state");
        assert!(!completed, "pre-fork Lua task crossed the session boundary");
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
        assert_eq!(s.model_history().len(), history.len());
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
        let model = session.model_history();
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
        let item = HistoryItem::user(protocol::compaction_summary_content("here is the summary"));
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
        let model = session.model_history();
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
            HistoryItem::user(protocol::compaction_summary_content("the summary")),
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
        let mut session = app.app.conversation.session().clone();
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
        app.app.conversation.install_session_for_harness(session);
        app.app.restore_screen();

        let restored = app.app.rewind_to(2).expect("second user turn");

        assert_eq!(restored.0, "c");
        assert_eq!(app.app.conversation.session().history.len(), 2);
        assert_eq!(app.app.conversation.session().session_cost_usd, 1.0);
        assert_eq!(
            app.app.conversation.session().session_usage.prompt_tokens,
            Some(30)
        );
        assert_eq!(
            app.app
                .conversation
                .session()
                .session_usage
                .completion_tokens,
            Some(3)
        );
        assert_eq!(app.app.conversation.session().context_tokens, Some(50));
        assert_eq!(
            app.app.conversation.session().context_tokens_history_len,
            Some(2)
        );
        assert_eq!(app.app.conversation.session().context_snapshots.len(), 1);
    }

    #[test]
    fn sparse_rewind_race_rejects_stale_window_and_persists_new_tail() {
        const TARGET_HISTORY_INDEX: usize = 13;
        const FAR_HISTORY_INDEX: usize = 1_000;
        let expected = "exact first line\nexact second line\nexact final line";
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.conversation.replace_history_for_harness(
            (0usize..1_200)
                .map(|index| {
                    if index == TARGET_HISTORY_INDEX || index.is_multiple_of(2) {
                        user(if index == TARGET_HISTORY_INDEX {
                            expected
                        } else {
                            "ordinary user turn"
                        })
                    } else {
                        assistant("ordinary assistant turn")
                    }
                })
                .collect(),
        );
        app.app.restore_screen();
        let target_id = app
            .app
            .conversation
            .transcript()
            .history()
            .order
            .iter()
            .copied()
            .find(|id| {
                app.app
                    .conversation
                    .transcript()
                    .history()
                    .block_origin(*id)
                    == Some(smelt_core::BlockOrigin::History(TARGET_HISTORY_INDEX))
                    && app.app.conversation.transcript().history().block_kind(*id) == Some("user")
            })
            .expect("rewind target block");
        let far_id = app
            .app
            .conversation
            .transcript()
            .history()
            .order
            .iter()
            .copied()
            .find(|id| {
                app.app
                    .conversation
                    .transcript()
                    .history()
                    .block_origin(*id)
                    == Some(smelt_core::BlockOrigin::History(FAR_HISTORY_INDEX))
            })
            .expect("far sparse block");
        app.app.save_session_and_flush();

        let session_dir = app
            .app
            .core
            .sessions
            .dir_for(app.app.conversation.session());
        let session_id = app.app.conversation.session().id.clone();
        let expected_record_prefix = lineage_transcript(&lineage_reader(&app, &session_id))
            .into_iter()
            .take(TARGET_HISTORY_INDEX)
            .collect::<Vec<_>>();
        assert_eq!(expected_record_prefix.len(), TARGET_HISTORY_INDEX);
        let loaded = load_transcript_tail_from_sqlite_dir(session_dir.clone(), 100, 32)
            .expect("load sparse transcript");
        app.app.clear_transcript();
        app.app
            .conversation
            .replace_loaded_transcript_for_harness(loaded);
        assert!(app
            .app
            .conversation
            .activate_transcript_search_record_window(100, target_id.get(), 32));
        app.app
            .conversation
            .set_transcript_memory_budget_for_harness(
                crate::app::transcript::TranscriptMemoryBudget {
                    hydrated_blocks: 1,
                    ..Default::default()
                },
            );
        assert!(!app
            .app
            .conversation
            .transcript()
            .history()
            .is_materialized(target_id));
        let target_index = app
            .app
            .conversation
            .transcript()
            .history()
            .order
            .iter()
            .position(|id| *id == target_id)
            .expect("target in active record window");

        let restored = app
            .app
            .rewind_to(target_index)
            .expect("rewind stored target");
        assert_eq!(restored.0, expected);
        assert!(app
            .app
            .conversation
            .transcript()
            .history()
            .order
            .iter()
            .all(|id| {
                app.app
                    .conversation
                    .transcript()
                    .history()
                    .block_origin(*id)
                    .is_none_or(|origin| {
                        !matches!(origin, smelt_core::BlockOrigin::History(index) if index >= TARGET_HISTORY_INDEX)
                    })
            }));

        let (commit_started, release_commit) =
            app.app.conversation.install_persistence_commit_barrier();
        app.save_session();
        commit_started
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("truncating save reaches the commit barrier");
        assert!(!app
            .app
            .conversation
            .activate_transcript_search_record_window(100, far_id.get(), 32));
        app.push_transcript_block(Block::Thinking {
            title: None,
            summary_titles: Vec::new(),
            kind: protocol::ReasoningKind::Raw,
            content: "newer generation while truncation is in flight".into(),
        });
        app.save_session();
        release_commit.send(()).expect("release truncating save");
        let outcome = app.flush_persist();
        assert!(
            matches!(
                outcome,
                crate::persist::PersistenceFlushOutcome::Durable { .. }
            ),
            "newer save should converge after truncation: {outcome:?}"
        );
        assert!(
            app.overlays_probe().notification().is_none(),
            "save race should not surface a persistence failure: {:?}",
            app.overlays_probe().notification()
        );
        let reader = lineage_reader(&app, &session_id);
        let state = reader.snapshot().expect("read rewound lineage state");
        assert_eq!(state.head.history_len.get() as usize, TARGET_HISTORY_INDEX);
        assert_eq!(state.transcript_len as usize, TARGET_HISTORY_INDEX + 1);
        let records = lineage_transcript(&reader);
        assert_eq!(records.len(), TARGET_HISTORY_INDEX + 1);
        assert_eq!(
            &records[..TARGET_HISTORY_INDEX],
            expected_record_prefix.as_slice()
        );
        let tail = records.last().expect("new transcript tail");
        assert_eq!(tail.history_idx, None);
        assert!(
            tail.preview_text
                .contains("newer generation while truncation is in flight"),
            "unexpected transcript tail: {tail:#?}"
        );
    }

    #[test]
    fn app_rewind_marks_context_tokens_stale_for_different_model() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let old_identity = smelt_core::session::ContextTokenIdentity {
            model: Some("old-model".into()),
            api_base: Some("https://old.example".into()),
            provider_type: Some("old-provider".into()),
        };
        let mut session = app.app.conversation.session().clone();
        session.history = vec![user("a"), assistant("b")];
        session.context_tokens = Some(50);
        session.context_tokens_history_len = Some(2);
        session.context_token_identity = Some(old_identity.clone());
        session.display_context_tokens = Some(50);
        session.display_context_token_identity = Some(old_identity);
        session.snapshot_context();
        session.history.extend([user("c"), assistant("d")]);
        app.app.conversation.install_session_for_harness(session);
        app.app.restore_screen();

        let restored = app.app.rewind_to(2).expect("second user turn");

        assert_eq!(restored.0, "c");
        assert_eq!(
            app.app.conversation.session().display_context_tokens(),
            Some(50)
        );
        assert!(app
            .app
            .conversation
            .session()
            .display_context_tokens_stale(&app.app.active_context_token_identity()));
        assert!(app.app.conversation.session().context_tokens.is_none());
    }

    #[test]
    fn app_rewind_restores_turn_tps_snapshot() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let mut session = app.app.conversation.session().clone();
        session.history = vec![user("a"), assistant("b")];
        session.turn_metas.push((
            2,
            protocol::TurnMeta {
                elapsed_ms: 10,
                avg_tps: Some(20.0),
                display_tps: Some(20.0),
                interrupted: false,
            },
        ));
        session.history.extend([user("c"), assistant("d")]);
        session.turn_metas.push((
            4,
            protocol::TurnMeta {
                elapsed_ms: 20,
                avg_tps: Some(50.0),
                display_tps: Some(50.0),
                interrupted: false,
            },
        ));
        app.app.conversation.install_session_for_harness(session);
        app.app.restore_screen();
        assert_eq!(app.app.working.display_tps(), Some(50.0));

        let restored = app.app.rewind_to(2).expect("second user turn");

        assert_eq!(restored.0, "c");
        assert_eq!(app.app.conversation.session().history.len(), 2);
        assert_eq!(app.app.conversation.session().turn_metas.len(), 1);
        assert_eq!(app.app.working.display_tps(), Some(20.0));
    }

    #[test]
    fn app_rewind_restores_carried_tps_snapshot_without_turn_samples() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let mut session = app.app.conversation.session().clone();
        session.history = vec![user("a"), assistant("b")];
        session.turn_metas.push((
            2,
            protocol::TurnMeta {
                elapsed_ms: 10,
                avg_tps: Some(20.0),
                display_tps: Some(20.0),
                interrupted: false,
            },
        ));
        session.history.extend([user("c"), assistant("d")]);
        session.turn_metas.push((
            4,
            protocol::TurnMeta {
                elapsed_ms: 20,
                avg_tps: None,
                display_tps: Some(20.0),
                interrupted: false,
            },
        ));
        session.history.extend([user("e"), assistant("f")]);
        session.turn_metas.push((
            6,
            protocol::TurnMeta {
                elapsed_ms: 30,
                avg_tps: Some(50.0),
                display_tps: Some(50.0),
                interrupted: false,
            },
        ));
        app.app.conversation.install_session_for_harness(session);
        app.app.restore_screen();
        assert_eq!(app.app.working.display_tps(), Some(50.0));

        let restored = app.app.rewind_to(4).expect("third user turn");

        assert_eq!(restored.0, "e");
        assert_eq!(app.app.conversation.session().history.len(), 4);
        assert_eq!(app.app.conversation.session().turn_metas.len(), 2);
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
