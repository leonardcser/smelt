use super::*;
use crate::app::session_load::OwnershipRecoveryEvent;
use protocol::{
    AssistantStep, Content, EngineEvent, HistoryAppend, HistoryAppendResult, HistoryItem, Role,
    TokenUsage, ToolInvocation, ToolOutcome,
};
use smelt_core::transcript_model::Block;

/// Saves `session` with `records` as its transcript straight to the app's store.
fn commit_session_fixture(
    app: &TestApp,
    session: &smelt_core::session::Session,
    records: Vec<smelt_store::StoredTranscriptBlock>,
) -> smelt_store::StoreHead {
    let mut commit = smelt_core::session::full_commit(session).unwrap();
    commit.transcript = Some(smelt_store::TranscriptRecordSuffix { start: 0, records });
    app.tui
        .core
        .sessions
        .store()
        .writer(&session.id)
        .unwrap()
        .commit(&smelt_core::session::store_identity(session), &commit)
        .unwrap()
}

#[test]
fn stored_session_info_counts_canonical_users_without_opening_history() {
    let mut app = TestApp::builder().build();
    let mut session = app.new_session();
    session
        .history
        .push(HistoryItem::user(Content::text("recorded request")));
    session.history.extend((0..4096).map(|index| {
        HistoryItem::note(protocol::HistoryNote::named_context(
            format!("context-{index}"),
            "hidden context",
        ))
    }));
    session
        .history
        .push(HistoryItem::user(protocol::compaction_summary_content(
            "summary",
        )));
    session
        .history
        .push(HistoryItem::user(Content::text("unrecorded request")));
    let id = session.id.clone();
    let transcript = crate::app::history::build_transcript_from_session(&app.tui.lua, &session);
    let record = transcript.history.block_records_with_ids_from(0).remove(0);
    commit_session_fixture(
        &app,
        &session,
        vec![
            smelt_core::transcript_model::transcript_block_row_with_block_idx(
                0,
                record.block_id.get(),
                &record.record,
            )
            .unwrap(),
        ],
    );
    assert!(app.resume_session(&id));
    app.ensure_writer_ready();
    assert!(app.tui.conversation.has_live_session());
    let perf_enabled = smelt_perf::perf::enabled();
    smelt_perf::perf::set_enabled(true);
    smelt_perf::perf::clear();
    let success = app.run_lua("for i = 1, 8 do assert(smelt.session.info().turn_count == 2) end");
    let opens = smelt_perf::perf::snapshot()
        .durations
        .iter()
        .find(|row| row.label == "session:live_store:open")
        .map_or(0, |row| row.count);
    smelt_perf::perf::set_enabled(perf_enabled);
    assert!(
        success,
        "stored counts must exclude summaries and include unrecorded canonical users"
    );
    assert_eq!(
        opens, 0,
        "session.info() must use its maintained count, not open history on the UI"
    );
}

#[test]
fn enter_dispatches_and_repaints_while_the_writer_is_paused() {
    let mut app = TestApp::builder().build();
    app.ensure_writer_ready();
    let release = app.pause_persistence();
    app.type_text("submitted request α");
    let started = std::time::Instant::now();
    app.press(KeyCode::Enter);
    // Storage stays paused, so any wait on it would run to the persistence deadline.
    assert!(started.elapsed() < crate::persist::DEFAULT_PERSISTENCE_DEADLINE);
    assert!(app.agent_running());
    assert_eq!(app.state().prompt_text, "");
    assert_eq!(
        app.drain_engine_sends()
            .iter()
            .filter(|command| matches!(command, protocol::UiCommand::StartTurn(_)))
            .count(),
        1
    );
    app.type_text("draft while saving");
    assert!(app.render_to_frame().text().contains("draft while saving"));

    release.send(()).unwrap();
    assert_eq!(
        app.flush_persist(),
        crate::persist::PersistenceFlushOutcome::Durable
    );
    assert_eq!(app.state().prompt_text, "draft while saving");
    let id = app.session_snapshot().id.clone();
    let reader = app.tui.core.sessions.reader(&id).unwrap();
    let head = reader.head().unwrap();
    let total = head.transcript_record_count.as_usize();
    let records = reader.transcript_range(0, total as u64).unwrap();
    assert!(records
        .iter()
        .any(|row| row.indexed_text.contains("submitted request α")));
}

#[test]
fn enter_dispatches_without_waiting_for_storage_at_any_history_size() {
    for retained in [0, 128, 4096] {
        let mut app = TestApp::builder().with_vim(false).build();
        if retained > 0 {
            let mut session = app.new_session();
            session.history = (0..retained)
                .map(|index| {
                    if index % 2 == 0 {
                        HistoryItem::user(Content::text(format!("retained request {index}")))
                    } else {
                        HistoryItem::Assistant(AssistantStep::terminal(
                            Some(Content::text("retained answer ".repeat(64))),
                            None,
                            Vec::new(),
                        ))
                    }
                })
                .collect();
            app.tui.load_session(session);
            app.ensure_writer_ready();
            save_full_history_fixture(&mut app);
            app.save_session_and_flush();
            let id = app.session_snapshot().id.clone();
            assert!(app.resume_session(&id));
        }
        app.ensure_writer_ready();
        let mut enters = Vec::new();
        for sample in 0..10 {
            let release = app.pause_persistence();
            app.type_text(&format!("measured request {sample}"));
            let entered = std::time::Instant::now();
            app.press(KeyCode::Enter);
            enters.push(entered.elapsed().as_micros());
            assert_eq!(
                app.drain_engine_sends()
                    .iter()
                    .filter(|command| matches!(command, protocol::UiCommand::StartTurn(_)))
                    .count(),
                1
            );
            release.send(()).unwrap();
            assert!(app.finish_turn());
            app.save_session_and_flush();
        }
        eprintln!("dispatch retained={retained} Enter_us={enters:?}");
        // Storage stays paused, so any wait on it would run to the persistence deadline.
        assert!(enters
            .iter()
            .all(|elapsed| *elapsed < crate::persist::DEFAULT_PERSISTENCE_DEADLINE.as_micros()));
    }
}

#[test]
fn file_expansion_accepts_input_before_reading_and_keeps_the_ui_responsive() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.ensure_writer_ready();
    let (input_started, release) = app.tui.conversation.submissions.pause_input();
    app.type_text("summarize @missing-responsive-fixture.txt");
    app.press(KeyCode::Esc);
    let started = std::time::Instant::now();
    app.press(KeyCode::Enter);
    assert!(
        started.elapsed() < Duration::from_millis(250),
        "Enter waited for file expansion"
    );
    input_started.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(app.tui.turn_submission_is_pending());
    assert_eq!(app.state().prompt_text, "");
    assert!(app
        .render_unsettled_silent()
        .text()
        .contains("summarize @missing-responsive-fixture.txt"));
    assert!(app
        .session_history()
        .iter()
        .all(|item| !matches!(item, HistoryItem::User { .. })));
    app.animate_with_virtual_clock();
    let mut dots = std::collections::HashSet::new();
    for _ in 0..8 {
        let started = std::time::Instant::now();
        let rendered = app.advance_animation(150);
        let frame = rendered.text();
        let dot = app
            .working_dot_style(&rendered)
            .expect("styled working dot");
        assert!(frame.contains('●'), "{frame}");
        dots.insert(dot);
        assert!(started.elapsed() < Duration::from_millis(250));
    }
    assert!(
        dots.len() >= 4,
        "the coal must keep breathing while the UI waits"
    );
    app.type_text("new draft");
    for key in [3, 12] {
        let started = std::time::Instant::now();
        app.press(KeyCode::F(key));
        app.render_unsettled_silent();
        assert!(started.elapsed() < Duration::from_millis(250));
    }
    release.send(()).unwrap();
    app.wait_for_turn_dispatch();
    assert!(app.agent_running());
    assert_eq!(app.state().prompt_text, "new draft");
}

#[test]
fn file_expansion_includes_turn_start_context_changes() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.ensure_writer_ready();
    assert!(app.run_lua(
        r#"
        smelt.events.on("turn_start", function()
            smelt.session.context_note("acceptance-hook", "request context from hook")
        end)
        "#
    ));
    let (input_started, release) = app.tui.conversation.submissions.pause_input();
    app.type_text("summarize @missing-context-fixture.txt");
    app.press(KeyCode::Esc);
    app.press(KeyCode::Enter);
    input_started.recv_timeout(Duration::from_secs(5)).unwrap();
    release.send(()).unwrap();
    app.wait_for_turn_dispatch();
    assert!(
        app.agent_running(),
        "turn-start context invalidated accepted input"
    );
    let sends = app.drain_engine_sends();
    let payload = sends
        .iter()
        .find_map(|command| match command {
            protocol::UiCommand::StartTurn(payload) => Some(payload),
            _ => None,
        })
        .unwrap();
    let history = &payload.history.items;
    assert!(history.iter().any(|item| matches!(item,
        HistoryItem::Note(protocol::HistoryNote::Context { name, text })
            if name == "acceptance-hook" && text == "request context from hook"
    )));
    app.assert_invariants();
}

#[test]
fn history_change_during_file_expansion_cancels_submission_and_keeps_callback_work() {
    {
        let mut app = TestApp::builder().with_vim(false).build();
        app.ensure_writer_ready();
        assert!(app.run_lua(
            r#"
            smelt.events.on("turn_start", function()
                smelt.session.context_note("retained-hook", "keep hook context")
            end)
            "#
        ));
        let (input_started, release) = app.tui.conversation.submissions.pause_input();
        app.type_text("summarize @missing-failure-fixture.txt");
        app.press(KeyCode::Esc);
        app.press(KeyCode::Enter);
        input_started.recv_timeout(Duration::from_secs(5)).unwrap();
        app.tui.push_block(Block::Text {
            content: "keep callback display".into(),
        });
        app.session_append_history(HistoryItem::Note(protocol::HistoryNote::Context {
            name: "later-context".into(),
            text: "keep later context".into(),
        }));
        release.send(()).unwrap();
        app.wait_for_turn_dispatch();
        assert!(!app.agent_running());
        assert!(!app.tui.turn_submission_is_pending());
        assert_eq!(
            app.state().prompt_text,
            "summarize @missing-failure-fixture.txt"
        );
        assert!(app.session_history().iter().any(|item| matches!(item,
            HistoryItem::Note(protocol::HistoryNote::Context { name, text })
                if name == "retained-hook" && text == "keep hook context"
        )));
        assert!(app
            .session_history()
            .iter()
            .all(|item| !matches!(item, HistoryItem::User { .. })));
        assert!(app
            .render_unsettled_silent()
            .text()
            .contains("keep callback display"));
        app.assert_invariants();
    }
}

#[test]
fn file_expansion_display_is_not_saved_until_promoted_in_place() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("fixture.txt"), "synthetic file content").unwrap();
    let mut app = TestApp::builder()
        .with_cwd(tmp.path())
        .with_vim(false)
        .build();
    app.start_submitted_turn("baseline");
    app.finish_turn();
    app.wait_for_turn_dispatch();
    app.save_session_and_flush();
    let id = app.session_snapshot().id.clone();
    let reader = app.tui.core.sessions.reader(&id).unwrap();
    let baseline = reader.head().unwrap();
    let (input_started, release) = app.tui.conversation.submissions.pause_input();
    app.type_text("summarize @fixture.txt");
    app.press(KeyCode::Esc);
    app.press(KeyCode::Enter);
    input_started.recv_timeout(Duration::from_secs(5)).unwrap();
    let block_id = app
        .tui
        .conversation
        .transcript()
        .history()
        .last_block_id()
        .unwrap();
    assert!(!app
        .tui
        .conversation
        .transcript()
        .history()
        .is_persisted_block(block_id));
    app.save_session_and_flush();
    let saved = reader.head().unwrap();
    assert_eq!(saved.history_len, baseline.history_len);
    assert_eq!(
        saved.transcript_record_count,
        baseline.transcript_record_count
    );
    assert!(!app
        .tui
        .core
        .files
        .has(&tmp.path().join("fixture.txt").to_string_lossy()));
    release.send(()).unwrap();
    app.wait_for_turn_dispatch();
    assert_eq!(
        app.tui.conversation.transcript().history().last_block_id(),
        Some(block_id)
    );
    assert!(app
        .tui
        .conversation
        .transcript()
        .history()
        .is_persisted_block(block_id));
    let sends = app.drain_engine_sends();
    let payload = sends
        .iter()
        .rev()
        .find_map(|command| match command {
            protocol::UiCommand::StartTurn(payload) => Some(payload),
            _ => None,
        })
        .unwrap();
    assert!(payload
        .input
        .provider_content()
        .text_content()
        .contains("synthetic file content"));
    assert!(app
        .tui
        .core
        .files
        .has(&tmp.path().join("fixture.txt").to_string_lossy()));
    app.assert_invariants();
}

#[test]
fn file_expansion_cancellation_and_force_ignore_stale_results() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("old.txt"), "old synthetic content").unwrap();
    for force in [false, true] {
        let mut app = TestApp::builder()
            .with_cwd(tmp.path())
            .with_vim(false)
            .build();
        app.ensure_writer_ready();
        let (input_started, release) = app.tui.conversation.submissions.pause_input();
        app.type_text("summarize @old.txt");
        app.press(KeyCode::Esc);
        app.press(KeyCode::Enter);
        input_started.recv_timeout(Duration::from_secs(5)).unwrap();
        if force {
            app.type_text("new request");
            for _ in 0..3 {
                app.press(KeyCode::Enter);
            }
            assert!(app.state().queued_inputs.is_empty());
            assert_eq!(
                app.state().prompt_text,
                "",
                "forcing restored cancelled input"
            );
            assert!(app.render_unsettled_silent().text().contains("new request"));
        } else {
            app.press(KeyCode::Esc);
            app.press(KeyCode::Esc);
            assert_eq!(app.state().prompt_text, "summarize @old.txt");
            assert!(!app.tui.turn_submission_is_pending());
        }
        release.send(()).unwrap();
        app.wait_for_turn_dispatch();
        assert!(!app
            .tui
            .core
            .files
            .has(&tmp.path().join("old.txt").to_string_lossy()));
        let inputs: Vec<_> = app
            .drain_engine_sends()
            .into_iter()
            .filter_map(|command| match command {
                protocol::UiCommand::StartTurn(payload) => {
                    Some(payload.input.provider_content().text_content().into_owned())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            inputs,
            if force {
                vec!["new request".to_string()]
            } else {
                Vec::new()
            }
        );
        app.assert_invariants();
    }
}

#[test]
fn scoped_preparation_errors_cannot_stop_another_turn_or_session() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.type_text("active request");
    app.press(KeyCode::Enter);
    app.wait_for_turn_dispatch();
    let turn_id = app.tui.conversation.active().unwrap().turn_id;
    let session_id = app.session_snapshot().id.clone();
    for stale in 0..2 {
        app.engine_event(EngineEvent::TurnPreparationError {
            turn_id: if stale == 0 { turn_id + 1 } else { turn_id },
            session_id: if stale == 1 {
                "another-session".into()
            } else {
                session_id.clone()
            },
            message: "stale request preparation failure".into(),
        });
        assert!(app.agent_running());
        assert!(!app
            .render_unsettled_silent()
            .text()
            .contains("stale request preparation failure"));
    }
    app.engine_event(EngineEvent::TurnPreparationError {
        turn_id,
        session_id,
        message: "matching request preparation failure".into(),
    });
    assert!(!app.agent_running());
    assert!(app
        .render_unsettled_silent()
        .text()
        .contains("matching request preparation failure"));
    app.assert_invariants();
}

#[test]
fn forced_queued_submission_starts_while_the_writer_is_paused() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.start_submitted_turn("initial request");
    app.drain_engine_sends();
    let release = app.pause_persistence();
    app.type_text("forced replacement");
    app.press(KeyCode::Enter);
    app.press(KeyCode::Enter);
    app.press(KeyCode::Enter);
    let frame = app.render_to_frame().text();
    let history = app.tui.conversation.transcript().history();
    let visible = history.order.iter().filter_map(|id| history.block(*id)).any(|block| {
        matches!(block, smelt_core::Block::User { text, .. } if text == "forced replacement")
    });
    let queued = app.state().queued_inputs.len();
    let started = app.agent_running();
    release.send(()).unwrap();
    assert!(visible, "forced message was not in the transcript: {frame}");
    assert_eq!(queued, 0, "forced message returned to a queue");
    assert!(started, "forced message did not start a visible turn");
}

#[test]
fn large_response_starts_follow_up_while_the_writer_is_paused() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.start_submitted_turn("initial request");
    app.drain_engine_sends();
    let turn_id = app.current_turn_id().unwrap();
    let body = format!("```\n{}\n```", "x".repeat(1024 * 1024));
    app.feed_one(SourceEvent::engine(EngineEvent::TextDelta {
        delta: body.clone(),
    }));
    let release = app.pause_persistence();

    app.feed_one(SourceEvent::engine(EngineEvent::HistoryAppended {
        turn_id,
        delta: protocol::CanonicalHistoryDelta::new(
            1,
            vec![HistoryItem::Assistant(protocol::AssistantStep::terminal(
                Some(Content::text(body)),
                None,
                Vec::new(),
            ))],
        ),
    }));
    app.feed_one(SourceEvent::engine(EngineEvent::TurnComplete {
        turn_id,
        history: None,
        meta: None,
    }));
    assert!(!app.agent_running());
    assert!(!app.working_probe().is_animating());
    app.type_text("follow-up request");
    app.press(KeyCode::Enter);
    let started = app.agent_running();
    release.send(()).unwrap();
    assert!(started);
    app.flush_persist();
    let reader = app
        .core_probe()
        .sessions
        .reader(&app.session_snapshot().id)
        .unwrap();
    let stored = reader.history_range(1, 3).unwrap();
    assert!(matches!(&stored[0], HistoryItem::Assistant(step)
        if step.content.as_ref().unwrap().text_content() == format!("```\n{}\n```", "x".repeat(1024 * 1024))));
    assert!(matches!(&stored[1], HistoryItem::User { content, .. }
        if content.text_content() == "follow-up request"));
}

#[test]
fn completion_dispatches_queued_work_while_the_writer_is_busy() {
    for queued in [false, true] {
        let mut app = TestApp::builder().build();
        app.start_submitted_turn("initial request");
        app.drain_engine_sends();
        if queued {
            app.push_queued_message("next request".into());
        }
        let release = app.pause_persistence();
        app.clear_actions();
        assert!(app.finish_turn());
        app.render_silent();
        assert_eq!(app.agent_running(), queued);
        assert!(app.state().queued_inputs.is_empty());
        if !queued {
            assert_eq!(
                app.working_probe().last_outcome(),
                Some(smelt_core::working::TurnOutcome::Done)
            );
        }
        assert_eq!(
            app.actions()
                .iter()
                .filter(|action| matches!(action, Action::EngineSend(command)
                    if matches!(command.as_ref(), protocol::UiCommand::StartTurn(_))))
                .count(),
            usize::from(queued)
        );
        app.type_text("draft after completion");
        assert!(app
            .render_to_frame()
            .text()
            .contains("draft after completion"));
        release.send(()).unwrap();
        app.flush_persist();
        assert_eq!(app.state().prompt_text, "draft after completion");
    }
}

fn loaded_session(app: &TestApp, id: &str) -> smelt_core::session::Session {
    crate::app::history::load_saved_session(&app.core_probe().sessions, id).expect("session saved")
}

fn session_reader(id: &str) -> smelt_store::SessionReader {
    smelt_core::session::storage().reader(id).unwrap()
}

/// How many saves of the current session reached the store.
fn durable_saves(app: &TestApp) -> u64 {
    app.tui
        .conversation
        .persistence_status()
        .and_then(|status| status.durable)
        .map_or(0, |durable| durable.seq)
}

fn save_full_history_fixture(app: &mut TestApp) {
    let transcript =
        crate::app::history::build_transcript_from_session(&app.tui.lua, &app.session_snapshot());
    app.tui
        .conversation
        .replace_transcript_from_history(transcript);
    app.save_session_and_flush();
}

fn save_and_close_full_history_fixture(mut app: TestApp) -> String {
    save_full_history_fixture(&mut app);
    let session_id = app.session_snapshot().id.clone();
    app.tui
        .shutdown_persist()
        .expect("close saved fixture persistence");
    assert!(
        app.core_probe().sessions.reader(&session_id).is_ok(),
        "closed fixture must be visible to another app"
    );
    session_id
}

fn saved_one_row_session(guard: &smelt_test_support::ProcessEnvironmentGuard) -> String {
    let mut app = TestApp::builder().build_with_test_home_guard(guard);
    app.session_append_history(HistoryItem::user(Content::text("persisted before resume")));
    save_and_close_full_history_fixture(app)
}

fn wait_for_session_load(app: &mut TestApp, id: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while app.session_snapshot().id != id {
        app.tui.drain_persist_reports();
        if let Some(event) = app.try_recv_app_event() {
            app.handle_app_event(event);
        } else {
            assert!(
                std::time::Instant::now() < deadline,
                "background session load did not complete"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    app.wait_for_session_lifecycle();
}

fn has_sticky_session_save_failure(app: &TestApp, session_id: &str) -> bool {
    app.overlays_probe()
        .notification()
        .is_some_and(|notification| {
            notification.lifetime.is_sticky()
                && matches!(
                    &notification.scope,
                    crate::app::NotificationScope::Operation(
                        crate::app::NotificationOperation::SessionPersistence(owner_session_id)
                    ) if owner_session_id == session_id
                )
        })
}

fn retry_persistence_via_lua(app: &mut TestApp) -> bool {
    app.eval_lua("return smelt.session.retry_persistence()")
        .unwrap()
}

fn request_audit_entry(request_id: u64) -> protocol::request_log::RequestLogEntry {
    protocol::request_log::RequestLogEntry {
        request_id,
        kind: "turn".into(),
        turn_id: Some(request_id),
        ask_id: None,
        history_len: Some(1),
        timestamp_ms: 1,
        provider_kind: "openai".into(),
        api_base: "https://api.example.test".into(),
        model: "model-a".into(),
        url: "https://api.example.test/v1/chat/completions".into(),
        http_status: Some(200),
        body: serde_json::json!({"model": "model-a"}),
        prompt_cache_key: None,
        stream: true,
        system_prompt: None,
        messages: None,
        tools: None,
        response: None,
        usage: None,
        cost_usd: None,
        tokens_per_sec: None,
        elapsed_ms: Some(10),
        attempt: 1,
        error: None,
        background: false,
    }
}

fn tool_history() -> Vec<HistoryItem> {
    vec![
        HistoryItem::user(Content::text("run the tool")),
        HistoryItem::Assistant(AssistantStep::with_invocations(
            Some(Content::text("tool completed")),
            None,
            Vec::new(),
            vec![ToolInvocation {
                call_id: "call-1".into(),
                name: "bash".into(),
                arguments: r#"{"command":"echo persisted"}"#.into(),
                result: ToolOutcome::new("persisted\n".into(), false, None),
                elapsed_ms: Some(12),
                called_at_ms: Some(1_742_573_823_000),
                provider: None,
            }],
        )),
    ]
}

fn assert_committed_tool_invocation(history: &[HistoryItem]) {
    let invocation = history
        .iter()
        .find_map(|item| match item {
            HistoryItem::Assistant(step) => step.invocations.first(),
            _ => None,
        })
        .expect("committed tool invocation persisted");

    assert_eq!(invocation.call_id, "call-1");
    assert_eq!(invocation.name, "bash");
    assert_eq!(invocation.arguments, r#"{"command":"echo persisted"}"#);
    assert_eq!(invocation.result.content, "persisted\n");
    assert!(!invocation.result.is_error);
    assert_eq!(invocation.elapsed_ms, Some(12));
    assert_eq!(invocation.called_at_ms, Some(1_742_573_823_000));
}

fn assert_model_history_tool_messages(messages: &[protocol::Message]) {
    let assistant = messages
        .iter()
        .find(|message| message.role == Role::Assistant && message.tool_calls.is_some())
        .expect("assistant tool call message restored");
    let tool_calls = assistant.tool_calls.as_ref().unwrap();
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].id, "call-1");
    assert_eq!(tool_calls[0].function.name, "bash");
    assert_eq!(
        tool_calls[0].function.arguments,
        r#"{"command":"echo persisted"}"#
    );

    let tool = messages
        .iter()
        .find(|message| {
            message.role == Role::Tool && message.tool_call_id.as_deref() == Some("call-1")
        })
        .expect("matching tool result message restored");
    assert_eq!(tool.content.as_ref().unwrap().text_content(), "persisted\n");
    assert!(!tool.is_error);
}

#[test]
fn session_save_notification_dismissal_uses_typed_scope() {
    let mut app = TestApp::builder().build();
    let session_id = app.session_snapshot().id.clone();

    app.notify_application_error_sticky(format!(
        "failed to save session {session_id}: unrelated diagnostic"
    ));
    app.dismiss_session_save_failure_notification(&session_id);
    assert!(
        app.overlays_probe().notification().is_some(),
        "matching copy without persistence scope must remain visible"
    );

    app.notify_session_save_failure(&session_id, "database busy");
    app.dismiss_session_save_failure_notification("another-session");
    assert!(has_sticky_session_save_failure(&app, &session_id));

    app.dismiss_session_save_failure_notification(&session_id);
    assert!(app.overlays_probe().notification().is_none());
}

#[test]
fn user_timestamp_survives_submission_save_resume_and_history_rebuild() {
    const SENT_AT_MS: u64 = 1_742_567_823_000;
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder()
            .with_wall_time(std::time::UNIX_EPOCH + std::time::Duration::from_millis(SENT_AT_MS))
            .build_with_test_home_guard(&guard);
        assert!(app.run_lua("smelt.settings.transcript.show_timestamps = false"));
        app.type_text("Preserve this message's submission time.");
        app.press(KeyCode::Enter);
        app.wait_for_turn_dispatch();
        let input = app
            .drain_engine_sends()
            .into_iter()
            .find_map(|command| match command {
                protocol::UiCommand::StartTurn(payload) => Some(payload.input),
                _ => None,
            })
            .expect("start turn dispatched");
        assert_eq!(input.sent_at_ms(), Some(SENT_AT_MS));
        let turn_id = app.current_turn_id().expect("submitted turn");
        app.feed_one(SourceEvent::engine(EngineEvent::TurnComplete {
            turn_id,
            history: None,
            meta: None,
        }));
        app.save_session_and_flush();
        let session_id = app.session_snapshot().id.clone();
        let saved = loaded_session(&app, &session_id);
        assert!(saved.history.iter().any(|item| matches!(
            item,
            HistoryItem::User {
                sent_at_ms: Some(SENT_AT_MS),
                ..
            }
        )));
        let rebuilt = crate::app::history::build_transcript_from_session(&app.tui.lua, &saved);
        assert!(rebuilt.history.order.iter().any(|id| matches!(
            rebuilt.history.block(*id),
            Some(Block::User {
                sent_at_ms: Some(SENT_AT_MS),
                ..
            })
        )));
        app.tui.shutdown_persist().expect("close saved session");
        session_id
    };

    let mut resumed = TestApp::builder()
        .with_wall_time(
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(SENT_AT_MS + 86_400_000),
        )
        .build_without_test_home_reset(&guard);
    resumed.load_session_by_id(&session_id);
    resumed.render_silent();
    let history = resumed.tui.conversation.transcript().history();
    assert!(history.order.iter().any(|id| matches!(
        history.block(*id),
        Some(Block::User {
            sent_at_ms: Some(SENT_AT_MS),
            ..
        })
    )));
    assert!(resumed.run_lua("assert(smelt.settings.transcript.show_timestamps == true)"));
}

#[test]
fn metadata_only_title_update_persists_after_clean_history_save() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    let session_id = app.session_snapshot().id.clone();

    app.session_append_history(HistoryItem::user(Content::text("persisted history")));
    app.save_session_and_flush();
    app.set_session_title("Renamed session".into(), "renamed-session".into(), None);
    app.save_session_and_flush();

    let loaded = loaded_session(&app, &session_id);
    assert_eq!(loaded.title.as_deref(), Some("Renamed session"));
    assert_eq!(loaded.slug.as_deref(), Some("renamed-session"));
    assert_eq!(loaded.history.len(), 1);
}

#[test]
fn fast_mode_only_change_is_saved_once() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    let session_id = app.session_snapshot().id.clone();
    app.session_append_history(HistoryItem::user(Content::text("persisted history")));
    app.save_session_and_flush();
    let before = durable_saves(&app);

    app.set_fast_mode(true);
    app.save_session_and_flush();

    assert_eq!(durable_saves(&app), before + 1);
    assert_eq!(loaded_session(&app, &session_id).fast_mode, Some(true));
}

#[test]
fn record_only_change_is_saved_once() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    let session_id = app.session_snapshot().id.clone();
    app.session_append_history(HistoryItem::user(Content::text("persisted history")));
    app.save_session_and_flush();
    let before = durable_saves(&app);
    let records_before = session_reader(&session_id)
        .head()
        .unwrap()
        .transcript_record_count;

    app.push_transcript_block(Block::Thinking {
        title: None,
        summary_titles: Vec::new(),
        kind: protocol::ReasoningKind::Raw,
        content: "record-only change".into(),
    });
    app.save_session_and_flush();

    assert_eq!(durable_saves(&app), before + 1);
    assert!(
        session_reader(&session_id)
            .head()
            .unwrap()
            .transcript_record_count
            > records_before
    );
}

#[test]
fn reasoning_summary_event_merges_durably_compacted_tail() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    app.start_turn(42);
    app.feed_one(SourceEvent::engine(EngineEvent::ReasoningPartFinished {
        id: "summary-1".into(),
        kind: protocol::ReasoningKind::Summary,
        title: Some("Inspecting the report".into()),
        content: String::new(),
    }));
    app.save_session_and_flush();

    app.drain_transcript_compaction_for_harness();
    let (len, original_id, materialized) = app
        .transcript_tail_state_for_harness()
        .expect("compacted reasoning summary");
    assert_eq!(len, 1);
    assert!(!materialized);
    app.set_transcript_memory_budget_for_harness(crate::app::transcript::TranscriptMemoryBudget {
        hydrated_blocks: 1,
        ..Default::default()
    });

    app.feed_one(SourceEvent::engine(EngineEvent::ReasoningPartFinished {
        id: "summary-2".into(),
        kind: protocol::ReasoningKind::Summary,
        title: Some("Planning the fix".into()),
        content: "The stored tail remains mergeable.".into(),
    }));
    app.render_silent();

    let (len, id, block) = app
        .with_pinned_transcript_blocks(&[original_id], |history| {
            (
                history.len(),
                history.last_block_id(),
                history.block(original_id).cloned(),
            )
        })
        .expect("hydrate merged summary");
    assert_eq!(len, 1);
    assert_eq!(id, Some(original_id));
    assert!(matches!(
        block,
        Some(Block::Thinking {
            title: Some(title),
            summary_titles,
            content,
            kind: protocol::ReasoningKind::Summary,
        }) if title == "Planning the fix"
            && summary_titles == ["Inspecting the report", "Planning the fix"]
            && content.snapshot() == "The stored tail remains mergeable."
    ));
}

#[test]
fn compacted_record_suffix_stays_saveable_after_inserting_a_prefix() {
    const RECORD_COUNT: usize = 600;

    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        for index in 0..RECORD_COUNT {
            app.push_transcript_block(Block::Text {
                content: format!("record {index}").into(),
            });
        }
        app.save_session_and_flush();
        app.session_snapshot().id.clone()
    };
    let mut app = TestApp::builder().build_without_test_home_reset(&guard);
    app.load_session_by_id(&session_id);
    let initial_record_start = {
        let history = app.conversation_probe().transcript().history();
        let first = history.order.first().copied().expect("loaded tail");
        history.stored_ref(first).unwrap().record_index
    };
    assert!(
        initial_record_start > 0,
        "test requires a sparse tail with a non-zero record base"
    );
    assert!(app
        .conversation_probe()
        .transcript()
        .history()
        .order
        .iter()
        .all(|id| !app
            .conversation_probe()
            .transcript()
            .history()
            .is_materialized(*id)));
    app.set_transcript_memory_budget_for_harness(crate::app::transcript::TranscriptMemoryBudget {
        hydrated_blocks: 1,
        ..Default::default()
    });

    app.insert_transcript_checkpoint_for_harness(
        0,
        0,
        Block::Text {
            content: "inserted prefix".into(),
        },
    );
    app.save_session();
    assert!(app
        .conversation_probe()
        .transcript()
        .history()
        .order
        .iter()
        .any(|id| !app
            .conversation_probe()
            .transcript()
            .history()
            .is_materialized(*id)));
    app.render_silent();
    app.flush_persist();
    assert!(
        !app.session_document_has_unflushed_work(),
        "shifted record save was not acknowledged: {:?}",
        app.overlays_probe().notification()
    );
    assert_eq!(
        app.conversation_probe().transcript().record_total_count(),
        Some(RECORD_COUNT + 1)
    );
    app.drain_transcript_compaction_for_harness();
    let inserted_id = app.conversation_probe().transcript().history().order[0];
    assert_eq!(
        app.conversation_probe()
            .transcript()
            .history()
            .stored_ref(inserted_id)
            .expect("inserted record compacted")
            .record_index,
        initial_record_start,
        "compaction lost the sparse tail's global record base"
    );
    app.require_transcript_record_resave_from_for_harness(0);
    app.set_fast_mode(true);
    app.save_session();
    app.render_silent();
    app.flush_persist();

    assert_eq!(
        app.conversation_probe()
            .transcript()
            .history()
            .record_dirty_from(),
        None
    );
    assert!(
        !app.session_document_has_unflushed_work(),
        "rewritten compacted record suffix remained dirty: {:?}",
        app.overlays_probe().notification()
    );
    assert!(
        !has_sticky_session_save_failure(&app, &session_id),
        "rewritten compacted record suffix failed to save: {:?}",
        app.overlays_probe().notification()
    );
    assert!(
        app.overlays_probe()
            .notification()
            .is_none_or(|notification| !notification
                .summary
                .contains("hydrate canonical transcript record suffix")),
        "record retry used stale persisted indices: {:?}",
        app.overlays_probe().notification()
    );

    let reader = session_reader(&session_id);
    let state = reader.state().unwrap();
    let durable_previews = reader
        .transcript_range(0, state.head.transcript_record_count.get())
        .unwrap()
        .into_iter()
        .map(|record| record.preview_text().to_owned())
        .collect::<Vec<_>>();
    let mut expected_previews = (0..RECORD_COUNT)
        .map(|index| format!("record {index}"))
        .collect::<Vec<_>>();
    expected_previews.insert(initial_record_start, "inserted prefix".into());
    assert_eq!(durable_previews, expected_previews);
    drop(app);

    let mut reloaded = TestApp::builder().build_without_test_home_reset(&guard);
    reloaded.load_session_by_id(&session_id);
    let ids = reloaded
        .conversation_probe()
        .transcript()
        .history()
        .order
        .clone();
    for id in ids.iter().copied() {
        let record_index = reloaded
            .conversation_probe()
            .transcript()
            .history()
            .stored_ref(id)
            .unwrap()
            .record_index;
        assert!(record_index < durable_previews.len());
    }
    assert!(reloaded
        .with_pinned_transcript_blocks(&ids, |_| ())
        .is_none());
    reloaded.render_silent();
    let reloaded_rows = reloaded
        .with_pinned_transcript_blocks(&ids, |history| {
            ids.iter()
                .map(|id| {
                    let stored = history.stored_ref(*id).unwrap();
                    (history.raw_text(*id).unwrap(), stored.record_index)
                })
                .collect::<Vec<_>>()
        })
        .expect("hydrate reloaded record tail");
    let reloaded_text = reloaded_rows
        .iter()
        .map(|(text, _)| text.clone())
        .collect::<Vec<_>>();
    let reloaded_expected = reloaded_rows
        .iter()
        .map(|(_, record_index)| durable_previews[*record_index].clone())
        .collect::<Vec<_>>();
    assert_eq!(reloaded_text, reloaded_expected);
}

#[test]
fn record_resave_preserves_semantic_history_links() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    let session_id = app.session_snapshot().id.clone();
    app.commit_request_history_item(
        HistoryItem::user(protocol::compaction_summary_content("retained summary")),
        Some(Block::Compacted {
            summary: "retained summary".into(),
            kept: Vec::new(),
        }),
    );
    let note = protocol::HistoryNote::process_status_event(
        protocol::ProcessStatusEvent::background_process_completed(
            "4242",
            Some(0),
            protocol::JobTermination::Exited,
        ),
    );
    app.commit_request_history_item(
        HistoryItem::note(note.clone()),
        app.history_note_to_block(&note),
    );
    assert!(!has_sticky_session_save_failure(&app, &session_id));

    app.require_transcript_record_resave_from_for_harness(0);
    app.save_session_and_flush();

    assert!(
        !has_sticky_session_save_failure(&app, &session_id),
        "record resave must preserve semantic origins: {:?}",
        app.overlays_probe().notification()
    );
}

#[test]
fn process_status_session_accepts_persisted_follow_up() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        app.handle_job_completed("4242".into(), Some(0), protocol::JobTermination::Exited);
        app.wait_for_turn_dispatch();
        let turn_id = app.current_turn_id().expect("process-status turn started");
        app.feed_one(SourceEvent::engine(EngineEvent::TurnComplete {
            turn_id,
            history: None,
            meta: None,
        }));
        app.save_session_and_flush();
        assert!(!app.session_is_read_only());
        app.session_snapshot().id.clone()
    };

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.load_session_by_id(&session_id);
    assert!(!resumed.session_is_read_only());

    resumed.start_submitted_turn("follow up after the agent finished");
    resumed.save_session_and_flush();

    assert!(
        !has_sticky_session_save_failure(&resumed, &session_id),
        "follow-up save failed: {:?}",
        resumed.overlays_probe().notification()
    );
    assert!(!resumed.session_is_read_only());
    assert!(loaded_session(&resumed, &session_id)
        .history
        .iter()
        .any(|item| {
            matches!(
                item,
                HistoryItem::User { content, .. }
                    if content.text_content() == "follow up after the agent finished"
            )
        }));
}

#[test]
fn no_op_save_writes_nothing() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    let session_id = app.session_snapshot().id.clone();
    app.session_append_history(HistoryItem::user(Content::text("persisted history")));
    app.save_session_and_flush();
    let before = durable_saves(&app);
    let head = session_reader(&session_id).head().unwrap();

    app.save_session_and_flush();

    assert_eq!(durable_saves(&app), before);
    assert_eq!(session_reader(&session_id).head().unwrap(), head);
}

#[test]
fn shutdown_flushes_latest_generation_after_in_flight_save() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);

    app.session_append_history(HistoryItem::user(Content::text("first generation")));
    app.save_session();
    assert!(app.session_document_has_unflushed_work());

    app.session_append_history(HistoryItem::user(Content::text("final generation")));
    app.save_session();
    assert!(app.session_document_has_unflushed_work());

    app.save_session_and_flush();

    let loaded = loaded_session(&app, &app.session_snapshot().id);
    assert_eq!(loaded.history.len(), 2);
    assert!(matches!(
        loaded.history.last(),
        Some(HistoryItem::User { content, .. }) if content.text_content() == "final generation"
    ));
}

#[test]
fn rewind_truncates_the_stored_suffix_across_restart() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    let session_id = app.session_snapshot().id.clone();

    app.commit_request_history_item(
        HistoryItem::user(Content::text("first prompt")),
        Some(Block::User {
            text: "first prompt".into(),
            image_labels: Vec::new(),
            command: false,
            sent_at_ms: None,
        }),
    );
    app.commit_request_history_item(
        HistoryItem::assistant(AssistantStep::terminal(
            Some(Content::text("first reply")),
            None,
            Vec::new(),
        )),
        Some(Block::Text {
            content: "first reply".into(),
        }),
    );
    let retained_block_len = app.transcript_block_count();
    let retained = session_reader(&session_id).state().unwrap();
    let retained_history_len = retained.head.history_len.as_usize();

    app.commit_request_history_item(
        HistoryItem::user(Content::text("discarded prompt")),
        Some(Block::User {
            text: "discarded prompt".into(),
            image_labels: Vec::new(),
            command: false,
            sent_at_ms: None,
        }),
    );
    app.commit_request_history_item(
        HistoryItem::assistant(AssistantStep::terminal(
            Some(Content::text("discarded reply")),
            None,
            Vec::new(),
        )),
        Some(Block::Text {
            content: "discarded reply".into(),
        }),
    );
    let sessions = smelt_core::session::storage();
    let store = sessions.store();
    assert!(session_reader(&session_id).head().unwrap().history_len > retained.head.history_len);

    app.rewind_to_history_index(Some(retained_history_len), false);
    app.save_session_and_flush();

    let rewound_reader = session_reader(&session_id);
    let rewound = rewound_reader.state().unwrap();
    let rewound_stats = rewound_reader.storage_stats(store).unwrap();
    assert_eq!(rewound.head, retained.head);
    assert_eq!(rewound_stats.history_rows, retained_history_len as u64);
    assert_eq!(
        rewound_stats.transcript_rows,
        retained.head.transcript_record_count.get()
    );
    drop(rewound_reader);
    drop(app);

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.load_session_by_id(&session_id);
    assert_eq!(session_reader(&session_id).head().unwrap(), retained.head);
    assert_eq!(resumed.session_message_count(), 2);
    assert_eq!(resumed.transcript_block_count(), retained_block_len);
}

#[test]
fn blocked_save_requires_explicit_retry() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    let session_id = app.session_snapshot().id.clone();
    app.session_append_history(HistoryItem::user(Content::text("baseline")));
    app.save_session_and_flush();
    app.inject_commit_failure(smelt_store::StoreError::UnsupportedSchema {
        found: i32::MAX,
        expected: 0,
    });
    app.session_append_history(HistoryItem::user(Content::text("retry me")));

    app.save_session_and_flush();
    assert!(app.session_document_has_unflushed_work());
    assert!(has_sticky_session_save_failure(&app, &session_id));

    assert!(retry_persistence_via_lua(&mut app));
    let outcome = app.flush_persist();
    assert!(
        !app.session_document_has_unflushed_work(),
        "retry flush left work pending: {outcome:?}; status: {:?}",
        app.conversation_probe().persistence_status()
    );
    let loaded = loaded_session(&app, &session_id);
    assert_eq!(loaded.history.len(), 2);
}

#[test]
fn environmental_failures_remain_dirty_until_explicit_retry() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    let session_id = app.session_snapshot().id.clone();
    app.session_append_history(HistoryItem::user(Content::text("baseline")));
    app.save_session_and_flush();

    for (index, message) in [
        "disk full while extending session database",
        "permission denied while writing session database",
        "session storage root is missing",
    ]
    .into_iter()
    .enumerate()
    {
        app.inject_commit_failure(smelt_store::StoreError::Io(std::io::Error::other(message)));
        app.session_append_history(HistoryItem::user(Content::text(message)));

        app.save_session_and_flush();

        assert!(app.session_document_has_unflushed_work());
        assert!(has_sticky_session_save_failure(&app, &session_id));
        assert!(app
            .overlays_probe()
            .notification()
            .is_some_and(|notification| notification.summary.contains(message)));
        assert_eq!(loaded_session(&app, &session_id).history.len(), index + 1);

        assert!(retry_persistence_via_lua(&mut app));
        app.flush_persist();
        assert!(!app.session_document_has_unflushed_work());
        assert_eq!(loaded_session(&app, &session_id).history.len(), index + 2);
    }
}

#[test]
fn turn_submitted_after_a_failed_save_saves_everything_once() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    let session_id = app.session_snapshot().id.clone();
    app.session_append_history(HistoryItem::user(Content::text("baseline")));
    app.save_session_and_flush();

    app.inject_commit_failure(smelt_store::StoreError::Io(std::io::Error::other(
        "database or disk is full",
    )));
    app.session_append_history(HistoryItem::user(Content::text("unsaved before Enter")));
    app.save_session_and_flush();
    assert!(app.session_document_has_unflushed_work());

    app.drain_engine_sends();
    app.type_text("submit after freeing space");
    app.press(KeyCode::Enter);

    assert!(app.agent_running());
    assert!(!app.session_is_read_only());
    assert_eq!(app.state().prompt_text, "");
    assert_eq!(
        app.session_snapshot()
            .history
            .iter()
            .filter(|item| matches!(
                item,
                HistoryItem::User { content, .. }
                    if content.text_content() == "submit after freeing space"
            ))
            .count(),
        1
    );
    app.set_session_title("Still writable".into(), "still-writable".into(), None);
    assert_eq!(
        app.session_snapshot().title.as_deref(),
        Some("Still writable")
    );
    let submitted = |app: &mut TestApp| {
        app.drain_engine_sends()
            .into_iter()
            .filter(|command| {
                matches!(
                    command,
                    protocol::UiCommand::StartTurn(payload)
                        if payload.input.provider_content().text_content()
                            == "submit after freeing space"
                )
            })
            .count()
    };
    assert_eq!(submitted(&mut app), 1);

    app.save_session_and_flush();
    let outcome = app.flush_persist();

    assert!(matches!(
        outcome,
        crate::persist::PersistenceFlushOutcome::Durable
    ));
    assert!(app.agent_running());
    assert!(!app.session_is_read_only());
    assert_eq!(submitted(&mut app), 0);
    let loaded = loaded_session(&app, &session_id);
    assert_eq!(loaded.title.as_deref(), Some("Still writable"));
    for expected in [
        "baseline",
        "unsaved before Enter",
        "submit after freeing space",
    ] {
        let matching_rows = loaded
            .history
            .iter()
            .filter(|item| {
                matches!(
                    item,
                    HistoryItem::User { content, .. } if content.text_content() == expected
                )
            })
            .count();
        assert_eq!(
            matching_rows, 1,
            "unexpected durable history: {:#?}",
            loaded.history
        );
    }
}

#[test]
fn failed_submit_save_is_written_once_after_explicit_retry() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    let session_id = app.session_snapshot().id.clone();
    app.session_append_history(HistoryItem::user(Content::text("baseline")));
    app.save_session_and_flush();

    app.inject_commit_failure(smelt_store::StoreError::Io(std::io::Error::other(
        "database or disk is full",
    )));
    app.drain_engine_sends();
    app.type_text("retry this exact turn");
    app.press(KeyCode::Enter);
    let submitted = |app: &mut TestApp| {
        app.drain_engine_sends()
            .into_iter()
            .filter(|command| {
                matches!(
                    command,
                    protocol::UiCommand::StartTurn(payload)
                        if payload.input.provider_content().text_content() == "retry this exact turn"
                )
            })
            .count()
    };

    assert!(app.agent_running());
    assert!(!app.session_is_read_only());
    assert_eq!(submitted(&mut app), 1);
    let _ = app.flush_persist();
    assert!(app
        .overlays_probe()
        .notification()
        .is_some_and(|notification| notification.summary.contains("/retry-save")));

    app.type_text("/retry-save");
    app.press(KeyCode::Enter);
    assert!(matches!(
        app.flush_persist(),
        crate::persist::PersistenceFlushOutcome::Durable
    ));
    assert!(app.agent_running());
    assert!(!app.session_is_read_only());
    assert_eq!(submitted(&mut app), 0);
    assert_eq!(
        loaded_session(&app, &session_id)
            .history
            .iter()
            .filter(|item| matches!(
                item,
                HistoryItem::User { content, .. } if content.text_content() == "retry this exact turn"
            ))
            .count(),
        1
    );
}

#[test]
fn lua_delete_returns_actionable_error_for_malicious_id() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    let target = engine::state_dir().join("must-not-delete");
    std::fs::create_dir_all(&target).unwrap();
    app.set_lua_string_global("DELETE_TARGET", target.to_string_lossy())
        .unwrap();

    let (ok, message): (bool, String) = app
        .eval_lua(
            "local ok, err = pcall(function() smelt.session.delete(DELETE_TARGET) end); return ok, tostring(err)",
        )
        .unwrap();

    assert!(!ok);
    assert!(message.contains("invalid session id"), "{message}");
    assert!(target.exists());
}

#[test]
fn shutdown_keeps_permanent_storage_failure_visible_and_dirty() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    let session_id = app.session_snapshot().id.clone();
    let store = smelt_core::session::storage().store().clone();
    let locks_dir = store.locks_dir();
    std::fs::create_dir_all(store.root()).unwrap();
    std::fs::write(&locks_dir, "permanently blocks directory creation").unwrap();
    app.session_append_history(HistoryItem::user(Content::text("cannot save")));

    app.save_session_and_flush();

    assert!(app.session_document_has_unflushed_work());
    assert!(app.overlays_probe().notification().is_some());
    assert!(smelt_core::session::storage().reader(&session_id).is_err());
}

#[test]
fn new_empty_session_is_not_saved() {
    let guard = test_home_guard();
    let app = TestApp::builder().build_with_test_home_guard(&guard);
    let session_id = app.session_snapshot().id.clone();
    drop(app);
    assert!(smelt_core::session::storage().reader(&session_id).is_err());
}

#[test]
fn request_audit_with_equal_body_and_response_keeps_both() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    let session_id = app.session_snapshot().id.clone();
    app.session_append_history(HistoryItem::user(Content::text("same bytes in two roles")));
    app.save_session_and_flush();
    let mut audit = request_audit_entry(43);
    audit.body = serde_json::json!({});
    audit.response = Some(protocol::request_log::RequestResponse {
        content: None,
        reasoning: None,
        tool_calls: None,
        raw: None,
        ..Default::default()
    });

    app.dispatch_host_call(engine::HostCall::RequestAudit {
        session_id: session_id.clone(),
        entry: Box::new(audit),
        payload_mode: smelt_store::RequestAuditPayloadMode::Full,
    });
    app.flush_persist();

    let reader = session_reader(&session_id);
    let requests = reader
        .requests(&smelt_store::RequestAuditQuery::default())
        .unwrap();
    assert_eq!(requests.len(), 1);
    let payloads = reader.request_payloads(requests[0].id).unwrap().unwrap();
    assert_eq!(payloads.body, Some(serde_json::json!({})));
    assert_eq!(payloads.response, Some(serde_json::json!({})));
}

#[test]
fn stale_request_audit_after_session_switch_is_dropped() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    app.session_append_history(HistoryItem::user(Content::text("old session")));
    app.save_session_and_flush();
    let old_id = app.session_snapshot().id.clone();

    app.reset_session();
    app.session_append_history(HistoryItem::user(Content::text("new session")));
    app.save_session_and_flush();
    let new_id = app.session_snapshot().id.clone();

    app.dispatch_host_call(engine::HostCall::RequestAudit {
        session_id: old_id.clone(),
        entry: Box::new(request_audit_entry(42)),
        payload_mode: smelt_store::RequestAuditPayloadMode::SUMMARY,
    });
    app.flush_persist();

    for id in [&old_id, &new_id] {
        assert!(session_reader(id)
            .requests(&smelt_store::RequestAuditQuery::default())
            .unwrap()
            .is_empty());
    }
}

#[test]
fn sparse_fork_publishes_a_complete_destination() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        app.session_append_history(HistoryItem::user(Content::text("fork source")));
        save_and_close_full_history_fixture(app)
    };
    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    assert!(
        resumed.load_session_by_id(&session_id),
        "source branch should load: {:?}",
        resumed.overlays_probe().notification()
    );

    resumed.fork_session();

    let fork_id = resumed.session_snapshot().id.clone();
    assert_ne!(
        fork_id,
        session_id,
        "fork should become active: {:?}",
        resumed.overlays_probe().notification()
    );
    let reader = session_reader(&fork_id);
    let stored = reader.state().unwrap();
    assert_eq!(stored.identity.id, fork_id);
    assert_eq!(
        stored.identity.parent_id.as_deref(),
        Some(session_id.as_str())
    );
    assert_eq!(reader.history_range(0, 1).unwrap().len(), 1);
}

#[test]
fn branch_switching_resumes_each_branch_at_its_exact_root() {
    let guard = test_home_guard();
    let source_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        app.session_append_history(HistoryItem::user(Content::text("shared root")));
        save_and_close_full_history_fixture(app)
    };
    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    assert!(
        resumed.load_session_by_id(&source_id),
        "source branch should load: {:?}",
        resumed.overlays_probe().notification()
    );
    resumed.fork_session();
    let fork_id = resumed.session_snapshot().id.clone();
    assert_ne!(
        fork_id,
        source_id,
        "fork should become active: {:?}",
        resumed.overlays_probe().notification()
    );
    let source_head = session_reader(&source_id).head().unwrap();
    resumed.session_append_history(HistoryItem::user(Content::text("fork only")));
    resumed.save_session_and_flush();
    let fork_head = session_reader(&fork_id).head().unwrap();

    assert!(
        resumed.load_session_by_id(&source_id),
        "source branch should load"
    );
    assert_eq!(resumed.session_snapshot().id, source_id);
    assert_eq!(resumed.session_message_count(), 1);
    assert_eq!(session_reader(&source_id).head().unwrap(), source_head);

    assert!(
        resumed.load_session_by_id(&fork_id),
        "fork branch should load: {:?}",
        resumed.overlays_probe().notification()
    );
    assert_eq!(resumed.session_snapshot().id, fork_id);
    assert_eq!(resumed.session_message_count(), 2);
    assert_eq!(session_reader(&fork_id).head().unwrap(), fork_head);
}

#[test]
fn deleting_source_branch_leaves_active_fork_intact() {
    let guard = test_home_guard();
    let source_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        app.session_append_history(HistoryItem::user(Content::text("shared fork history")));
        save_and_close_full_history_fixture(app)
    };
    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    assert!(
        resumed.load_session_by_id(&source_id),
        "source branch should load: {:?}",
        resumed.overlays_probe().notification()
    );
    resumed.fork_session();
    let fork_id = resumed.session_snapshot().id.clone();
    assert_ne!(
        fork_id,
        source_id,
        "fork should become active: {:?}",
        resumed.overlays_probe().notification()
    );
    resumed
        .set_lua_string_global("SOURCE_SESSION_ID", source_id.clone())
        .unwrap();

    resumed
        .run_lua_result("smelt.session.delete(SOURCE_SESSION_ID)")
        .expect("delete source branch through the user-facing session API");

    assert!(
        smelt_core::session::storage().reader(&source_id).is_err(),
        "deleted source branch is no longer visible"
    );
    assert_eq!(resumed.session_snapshot().id, fork_id);
    let fork = session_reader(&fork_id);
    assert_eq!(
        fork.history_range(0, 1).unwrap(),
        vec![HistoryItem::user(Content::text("shared fork history"))]
    );
}

#[test]
fn transcript_save_preparation_failure_blocks_retry_loop_and_session_replacement() {
    const RECORD_COUNT: usize = 600;

    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    for index in 0..RECORD_COUNT {
        app.push_transcript_block(Block::Text {
            content: format!("persisted record {index}").into(),
        });
    }
    app.save_session_and_flush();
    app.drain_transcript_compaction_for_harness();
    app.set_transcript_memory_budget_for_harness(crate::app::transcript::TranscriptMemoryBudget {
        hydrated_blocks: 1,
        ..Default::default()
    });
    app.render_silent();

    let session_id = app.session_snapshot().id.clone();
    let sessions = smelt_core::session::storage();
    app.require_transcript_record_resave_from_for_harness(0);
    app.set_fast_mode(true);
    app.set_transcript_store_for_harness(Some(sessions.address("0".repeat(32))));

    app.save_session();
    app.render_silent();

    assert!(app.session_document_has_unflushed_work());
    assert!(app
        .overlays_probe()
        .notification()
        .is_some_and(|notification| {
            notification
                .summary
                .contains("hydrate canonical transcript record suffix")
        }));
    assert!(matches!(
        app.flush_persist(),
        crate::persist::PersistenceFlushOutcome::Failed(cause)
            if cause.message.contains("hydrate canonical transcript record suffix")
    ));

    app.set_transcript_store_for_harness(Some(sessions.address(session_id.clone())));
    assert!(
        !app.load_session_by_id(&session_id),
        "loading must report that destination activation was refused"
    );
    assert_eq!(app.session_snapshot().id, session_id);
    app.reset_session();
    assert_eq!(
        app.session_snapshot().id,
        session_id,
        "a blocked save must prevent session replacement"
    );
    for _ in 0..3 {
        app.render_normal_to(&mut std::io::sink());
    }
    assert!(
        app.session_document_has_unflushed_work(),
        "automatic frames retried a blocked save"
    );

    assert!(retry_persistence_via_lua(&mut app));
    app.render_silent();
    app.flush_persist();
    assert!(!app.session_document_has_unflushed_work());
    assert!(!has_sticky_session_save_failure(&app, &session_id));
    assert_eq!(
        session_reader(&session_id)
            .state()
            .unwrap()
            .head
            .transcript_record_count
            .get(),
        RECORD_COUNT as u64
    );
}

#[test]
fn current_compacted_read_only_session_forks_without_hydrating_or_cloning_history() {
    const ROW_COUNT: usize = 700;

    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    let payload = "x".repeat(8 * 1024);
    for index in 0..ROW_COUNT {
        app.session_append_history(HistoryItem::user(Content::text(format!(
            "fork source row {index}\nexact suffix {index} {payload}"
        ))));
    }
    app.restore_screen();
    app.save_session();
    let persistence_deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        match app.flush_persist() {
            crate::persist::PersistenceFlushOutcome::Durable => break,
            crate::persist::PersistenceFlushOutcome::Deadline
                if std::time::Instant::now() < persistence_deadline => {}
            outcome => panic!("large fork fixture persistence failed: {outcome:?}"),
        }
    }
    app.drain_transcript_compaction_for_harness();
    app.set_transcript_memory_budget_for_harness(crate::app::transcript::TranscriptMemoryBudget {
        hydrated_blocks: 1,
        ..Default::default()
    });
    app.render_silent();

    let source_id = app.session_snapshot().id.clone();
    let source = session_reader(&source_id);
    let source_state = source.state().unwrap();
    let source_history = source.history_range(0, ROW_COUNT as u64).unwrap();
    let source_records = source
        .transcript_range(0, source_state.head.transcript_record_count.get())
        .unwrap();
    drop(source);
    assert!(
        app.conversation_probe()
            .transcript()
            .memory_snapshot()
            .hydrated_blocks
            < source_records.len()
    );

    let unsaved = HistoryItem::user(Content::text("preserved read-only suffix"));
    app.inject_commit_failure(smelt_store::StoreError::Owned {
        session_id: String::new(),
    });
    app.session_append_history(unsaved.clone());
    app.save_session_and_flush();
    assert!(app.session_is_read_only());
    assert!(app.session_document_has_unflushed_work());

    let (_, allocated_before) = smelt_perf::alloc::thread_snapshot();
    app.tui.fork_session();
    let (_, allocated_after) = smelt_perf::alloc::thread_snapshot();
    let allocated_bytes = allocated_after.saturating_sub(allocated_before);

    assert!(
        allocated_bytes <= 4 * 1024 * 1024,
        "current compacted fork allocated {allocated_bytes} bytes on the UI thread"
    );
    app.wait_for_session_lifecycle();
    let fork_id = app.session_snapshot().id.clone();
    assert_ne!(fork_id, source_id);
    assert!(
        !has_sticky_session_save_failure(&app, &fork_id),
        "fork failed to save: {:?}",
        app.overlays_probe().notification()
    );
    let fork = session_reader(&fork_id);
    let fork_state = fork.state().unwrap();
    assert_eq!(
        fork.history_range(0, ROW_COUNT as u64).unwrap(),
        source_history
    );
    assert_eq!(
        fork.history_range(ROW_COUNT as u64, ROW_COUNT as u64 + 1)
            .unwrap(),
        vec![unsaved]
    );
    assert_eq!(
        fork.transcript_range(0, fork_state.head.transcript_record_count.get())
            .unwrap(),
        source_records
    );
    drop(fork);

    let retained_source = session_reader(&source_id);
    let retained_state = retained_source.state().unwrap();
    assert_eq!(
        retained_source.history_range(0, ROW_COUNT as u64).unwrap(),
        source_history
    );
    assert_eq!(
        retained_source
            .transcript_range(0, retained_state.head.transcript_record_count.get())
            .unwrap(),
        source_records
    );
    drop(retained_source);

    app.load_session_by_id(&source_id);
    assert_eq!(app.session_snapshot().id, source_id);
}

#[test]
fn large_sparse_fork_preserves_every_canonical_history_and_record_row() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        for index in 0..700 {
            app.session_append_history(HistoryItem::user(Content::text(format!(
                "fork source row {index}\nexact suffix {index}"
            ))));
        }
        app.restore_screen();
        app.save_session_and_flush();
        app.session_snapshot().id.clone()
    };
    let source = session_reader(&session_id);
    let source_state = source.state().unwrap();
    let source_history = source.history_range(0, 700).unwrap();
    let source_records = source
        .transcript_range(0, source_state.head.transcript_record_count.get())
        .unwrap();
    assert_eq!(source_history.len(), 700);
    assert_eq!(source_records.len(), 700);

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    assert!(resumed.load_session_by_id(&session_id));
    resumed.set_transcript_memory_budget_for_harness(
        crate::app::transcript::TranscriptMemoryBudget {
            hydrated_blocks: 1,
            ..Default::default()
        },
    );
    resumed.render_silent();
    assert!(
        resumed
            .conversation_probe()
            .transcript()
            .memory_snapshot()
            .hydrated_blocks
            < source_records.len()
    );

    let (_, allocated_before) = smelt_perf::alloc::thread_snapshot();
    resumed.tui.fork_session();
    let (_, allocated_after) = smelt_perf::alloc::thread_snapshot();
    let allocated_bytes = allocated_after.saturating_sub(allocated_before);

    assert!(
        allocated_bytes <= 4 * 1024 * 1024,
        "large visible fork allocated {allocated_bytes} bytes on the UI thread"
    );
    resumed.wait_for_session_lifecycle();
    let fork_id = resumed.session_snapshot().id.clone();
    assert_ne!(fork_id, session_id);
    let fork = session_reader(&fork_id);
    let fork_state = fork.state().unwrap();
    assert_eq!(fork_state.head, source_state.head);
    assert_eq!(fork.history_range(0, 700).unwrap(), source_history);
    assert_eq!(
        fork.transcript_range(0, fork_state.head.transcript_record_count.get())
            .unwrap(),
        source_records
    );

    for target in [&session_id, &fork_id] {
        resumed
            .set_lua_string_global("__switch_target", target.clone())
            .unwrap();
        assert!(resumed.run_lua("smelt.session.load(__switch_target)"));
        resumed.wait_for_session_lifecycle();
        assert_eq!(resumed.session_snapshot().id.as_str(), target.as_str());
        assert_eq!(resumed.session_message_count(), 700);
    }
}

#[test]
fn shutdown_flushes_record_only_transcript_blocks() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    let session_id = app.session_snapshot().id.clone();

    app.push_transcript_block(Block::Thinking {
        title: None,
        summary_titles: Vec::new(),
        kind: protocol::ReasoningKind::Raw,
        content: "record-only interrupted thinking".into(),
    });
    app.save_session_and_flush();

    let reader = session_reader(&session_id);
    let state = reader.state().unwrap();
    let rows = reader
        .transcript_range(0, state.head.transcript_record_count.get())
        .unwrap();
    assert!(
        rows.iter().any(|row| row
            .preview_text()
            .contains("record-only interrupted thinking")),
        "record-only transcript block should be durable: {rows:#?}"
    );
}

#[test]
fn record_resume_interrupt_save_compacts_and_appends_again() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        app.commit_request_history_item(
            HistoryItem::user(Content::text("sparse record prompt")),
            Some(Block::User {
                text: "sparse record prompt".into(),
                image_labels: Vec::new(),
                command: false,
                sent_at_ms: None,
            }),
        );
        app.push_transcript_block(Block::Text {
            content: "persisted before sparse rewrite".into(),
        });
        app.save_session_and_flush();
        app.session_snapshot().id.clone()
    };

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.load_session_by_id(&session_id);
    assert_eq!(
        resumed
            .conversation_probe()
            .transcript()
            .record_total_count(),
        Some(2)
    );
    resumed.start_turn(3030);
    resumed.feed_one(SourceEvent::engine(EngineEvent::TextDelta {
        delta: "interrupted after sparse record load".into(),
    }));
    resumed.cancel();
    resumed.save_session_and_flush();
    assert!(
        resumed.overlays_probe().notification().is_none(),
        "sparse record save should not surface an integrity failure: {:?}",
        resumed.overlays_probe().notification()
    );
    drop(resumed);

    let reader = session_reader(&session_id);
    let state = reader.state().unwrap();
    assert_eq!(state.head.transcript_record_count.get(), 3);
    assert_eq!(
        reader
            .transcript_range(0, state.head.transcript_record_count.get())
            .unwrap()
            .len(),
        3
    );
    drop(reader);

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.load_session_by_id(&session_id);
    resumed.push_transcript_block(Block::Thinking {
        title: None,
        summary_titles: Vec::new(),
        content: "append after dense reconciliation".into(),
        kind: protocol::ReasoningKind::Raw,
    });
    resumed.save_session_and_flush();
    assert!(
        resumed.overlays_probe().notification().is_none(),
        "dense append save failed: {:?}",
        resumed.overlays_probe().notification()
    );

    let reader = session_reader(&session_id);
    let state = reader.state().unwrap();
    let rows = reader
        .transcript_range(0, state.head.transcript_record_count.get())
        .unwrap();
    assert_eq!(rows.len(), 4);
    assert!(rows.iter().any(|row| row
        .preview_text()
        .contains("append after dense reconciliation")));
}

#[test]
fn store_backed_resume_preserves_context_token_identity() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        app.session_append_history(HistoryItem::user(Content::text("token identity prompt")));
        app.record_visible_token_usage(TokenUsage {
            context_tokens: Some(1234),
            ..Default::default()
        });
        save_and_close_full_history_fixture(app)
    };

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    assert!(
        resumed.load_session_by_id(&session_id),
        "resume failed: {:?}",
        resumed
            .overlays_probe()
            .notification()
            .map(|notification| notification.summary.as_str())
    );
    assert_eq!(resumed.session_snapshot().id, session_id);
    let identity = resumed.active_context_token_identity();

    assert_eq!(
        resumed.session_snapshot().display_context_tokens(),
        Some(1234)
    );
    assert!(!resumed
        .conversation_probe()
        .session()
        .display_context_tokens_stale(&identity));
}

#[test]
fn store_backed_resume_uses_provider_snapshot_for_pre_request_compaction() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        for index in 0..3 {
            app.commit_request_history_item(
                HistoryItem::user(Content::text(format!("old user {index}"))),
                None,
            );
            app.commit_request_history_item(
                HistoryItem::assistant(AssistantStep::terminal(
                    Some(Content::text(format!("old assistant {index}"))),
                    None,
                    Vec::new(),
                )),
                None,
            );
        }
        app.record_visible_token_usage(TokenUsage {
            context_tokens: Some(100),
            ..Default::default()
        });
        save_and_close_full_history_fixture(app)
    };

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.load_session_by_id(&session_id);
    assert!(resumed.conversation_probe().has_live_session());
    assert!(resumed.tui.conversation.session().history.is_empty());

    let mut settings = resumed.core_probe().config.settings.clone();
    settings.auto_compact = true;
    settings.compact_threshold = 0.8;
    settings.compact_keep_recent_groups = 1.0;
    resumed.set_settings_for_harness(settings);
    resumed.set_context_window(Some(1_000));
    resumed.start_turn(42);

    let messages = protocol::history_to_messages(&resumed.model_history());
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    resumed.dispatch_host_call(engine::HostCall::PrepareRequest {
        turn_id: resumed.current_turn_id().expect("active request turn"),
        messages: engine::PreparedRequestMessages::model_only(messages),
        estimated_tokens: 2_000,
        reply: tx,
    });

    let sends = resumed.drain_engine_sends();
    assert!(
        sends
            .iter()
            .all(|command| !matches!(command, protocol::UiCommand::EngineAsk { .. })),
        "provider snapshot below the threshold should prevent compaction: {sends:?}"
    );
    assert!(matches!(
        rx.try_recv().expect("prepare request reply"),
        engine::HostRequestDecision::Continue
    ));
    assert_eq!(resumed.session_snapshot().context_tokens, Some(100));
    assert_eq!(
        resumed.session_snapshot().context_tokens_history_len,
        Some(6)
    );
}

#[test]
fn store_backed_usage_records_canonical_history_coordinate() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        app.session_append_history(HistoryItem::user(Content::text("stored prompt")));
        save_and_close_full_history_fixture(app)
    };

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.load_session_by_id(&session_id);
    resumed.session_append_history(HistoryItem::assistant(AssistantStep::terminal(
        Some(Content::text("live response")),
        None,
        Vec::new(),
    )));
    resumed.record_visible_token_usage(TokenUsage {
        context_tokens: Some(200),
        ..Default::default()
    });
    resumed.save_session_and_flush();

    let stored = session_reader(&session_id).state().unwrap();
    assert_eq!(stored.head.history_len.get(), 2);
    assert_eq!(stored.metadata.context_tokens, Some(200));
    assert_eq!(stored.metadata.context_tokens_history_len, Some(2));
}

#[test]
fn successful_compactions_remain_after_canonical_transcript_rebuild() {
    fn append_exchange(app: &mut TestApp, label: &str) {
        app.commit_request_history_item(
            HistoryItem::user(Content::text(format!("{label} user"))),
            Some(Block::User {
                text: format!("{label} user"),
                image_labels: Vec::new(),
                command: false,
                sent_at_ms: None,
            }),
        );
        app.commit_request_history_item(
            HistoryItem::assistant(AssistantStep::terminal(
                Some(Content::text(format!("{label} assistant"))),
                None,
                Vec::new(),
            )),
            Some(Block::Text {
                content: format!("{label} assistant").into(),
            }),
        );
    }

    fn compaction_positions(app: &mut TestApp) -> Vec<(String, Option<usize>)> {
        let ids = {
            let history = app.conversation_probe().transcript().history();
            (0..history.len())
                .filter_map(|block_index| {
                    let id = history.block_id_at(block_index)?;
                    (history.block_kind(id) == Some("compacted")).then_some(id)
                })
                .collect::<Vec<_>>()
        };
        if app.with_pinned_transcript_blocks(&ids, |_| ()).is_none() {
            app.render_silent();
        }
        app.with_pinned_transcript_blocks(&ids, |history| {
            ids.iter()
                .map(|id| {
                    let Block::Compacted { summary, .. } = history
                        .block(*id)
                        .expect("pinned compaction block is hydrated")
                    else {
                        panic!("compaction id changed kind after hydration");
                    };
                    let history_index = match history.block_origin(*id) {
                        Some(smelt_core::BlockOrigin::Checkpoint { history_index }) => {
                            Some(history_index)
                        }
                        _ => None,
                    };
                    (summary.clone(), history_index)
                })
                .collect()
        })
        .expect("compaction blocks hydrate from the session store")
    }

    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        append_exchange(&mut app, "old");
        append_exchange(&mut app, "middle");
        assert!(app.run_lua(
            r#"assert(smelt.session.checkpoint({
                summary = "first checkpoint",
                first_live_message_index = 2,
                tokens_before = 100,
            }))"#,
        ));

        append_exchange(&mut app, "recent");
        assert!(app.run_lua(
            r#"assert(smelt.session.checkpoint({
                summary = "second checkpoint",
                first_live_message_index = 3,
                tokens_before = 120,
            }))"#,
        ));

        assert_eq!(
            compaction_positions(&mut app),
            vec![
                ("first checkpoint".into(), Some(4)),
                ("second checkpoint".into(), Some(6)),
            ]
        );
        app.save_session_and_flush();
        let session_id = app.session_snapshot().id.clone();
        let loaded = loaded_session(&app, &session_id);
        assert_eq!(
            loaded
                .checkpoint_events
                .iter()
                .map(|event| {
                    (
                        event.summary.as_ref(),
                        event.first_live_index,
                        event.completed_at_history_len,
                    )
                })
                .collect::<Vec<_>>(),
            vec![("first checkpoint", 2, 4), ("second checkpoint", 4, 6)]
        );
        let lua = crate::lua::LuaRuntime::new();
        let rebuilt = crate::app::history::build_transcript_from_session(&lua, &loaded);
        let rebuilt_positions = rebuilt
            .history
            .order
            .iter()
            .filter_map(|id| {
                let Block::Compacted { summary, .. } = rebuilt.history.block(*id)? else {
                    return None;
                };
                let smelt_core::BlockOrigin::Checkpoint { history_index } =
                    rebuilt.history.block_origin(*id)?
                else {
                    return None;
                };
                Some((summary.as_str(), history_index))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            rebuilt_positions,
            vec![("first checkpoint", 4), ("second checkpoint", 6)]
        );
        session_id
    };

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.load_session_by_id(&session_id);
    assert_eq!(
        compaction_positions(&mut resumed),
        vec![
            ("first checkpoint".into(), Some(4)),
            ("second checkpoint".into(), Some(6)),
        ]
    );
}

#[test]
fn resumed_rewind_restores_prior_turn_context_before_next_request() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        for (turn_id, prompt, reply, context_tokens) in [
            (1, "first prompt", "first reply", 100),
            (2, "second prompt", "second reply", 250),
        ] {
            app.commit_request_history_item(
                HistoryItem::user(Content::text(prompt)),
                Some(Block::User {
                    text: prompt.into(),
                    image_labels: Vec::new(),
                    command: false,
                    sent_at_ms: None,
                }),
            );
            app.commit_request_history_item(
                HistoryItem::assistant(AssistantStep::terminal(
                    Some(Content::text(reply)),
                    None,
                    Vec::new(),
                )),
                Some(Block::Text {
                    content: reply.into(),
                }),
            );
            app.start_turn(turn_id);
            app.feed_one(SourceEvent::engine(EngineEvent::TokenUsage {
                usage: TokenUsage {
                    context_tokens: Some(context_tokens),
                    ..Default::default()
                },
                tokens_per_sec: None,
                cost_usd: None,
                background: false,
            }));
            app.feed_one(SourceEvent::engine(EngineEvent::TurnComplete {
                turn_id,
                history: None,
                meta: None,
            }));
        }
        app.save_session_and_flush();
        app.session_snapshot().id.clone()
    };

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.load_session_by_id(&session_id);
    resumed.set_terminal_size(80, 24);
    resumed.set_context_window(Some(1_000));
    assert!(resumed.conversation_probe().has_live_session());
    resumed.render_silent();

    resumed.rewind_to_history_index(Some(2), false);
    resumed.wait_for_rewind();
    assert_eq!(resumed.session_message_count(), 2);
    assert_eq!(resumed.session_snapshot().context_tokens, Some(100));
    assert_eq!(
        resumed.session_snapshot().context_tokens_history_len,
        Some(2)
    );
    assert_eq!(
        resumed.session_snapshot().display_context_tokens(),
        Some(100)
    );
    assert!(resumed.run_lua("assert(smelt.session.context_tokens() == 100)"));
    let rewound_frame = resumed.render_to_frame().text();
    assert!(
        rewound_frame.contains("100 (10%)"),
        "rewound context pill is missing:\n{rewound_frame}"
    );

    resumed.type_text("replacement prompt");
    resumed.press(crossterm::event::KeyCode::Enter);
    assert_eq!(
        resumed.session_snapshot().display_context_tokens(),
        Some(100)
    );
    let submitted_frame = resumed.render_to_frame().text();
    assert!(
        submitted_frame.contains("100 (10%)"),
        "context pill disappeared after submission:\n{submitted_frame}"
    );

    resumed.wait_for_turn_dispatch();
    let mut settings = resumed.core_probe().config.settings.clone();
    settings.auto_compact = true;
    settings.compact_threshold = 0.8;
    resumed.set_settings_for_harness(settings);
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    let turn_id = resumed.current_turn_id().expect("active request turn");
    let history = resumed.model_history();
    resumed.dispatch_host_call(engine::HostCall::PrepareRequest {
        turn_id,
        messages: engine::PreparedRequestMessages::model_only(protocol::history_to_messages(
            &history,
        )),
        estimated_tokens: 2_000,
        reply: tx,
    });

    let sends = resumed.drain_engine_sends();
    assert!(
        sends
            .iter()
            .all(|command| !matches!(command, protocol::UiCommand::EngineAsk { .. })),
        "restored context below the threshold should prevent compaction: {sends:?}"
    );
    assert!(matches!(
        rx.try_recv().expect("prepare request reply"),
        engine::HostRequestDecision::Continue
    ));
}

#[test]
fn interrupted_turn_rewind_save_resume_restores_prior_context_tokens() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        app.commit_request_history_item(
            HistoryItem::user(Content::text("first prompt")),
            Some(Block::User {
                text: "first prompt".into(),
                image_labels: Vec::new(),
                command: false,
                sent_at_ms: None,
            }),
        );
        app.commit_request_history_item(
            HistoryItem::assistant(AssistantStep::terminal(
                Some(Content::text("first reply")),
                None,
                Vec::new(),
            )),
            Some(Block::Text {
                content: "first reply".into(),
            }),
        );
        app.start_turn(1);
        app.feed_one(SourceEvent::engine(EngineEvent::TokenUsage {
            usage: TokenUsage {
                context_tokens: Some(100),
                ..Default::default()
            },
            tokens_per_sec: None,
            cost_usd: None,
            background: false,
        }));
        app.feed_one(SourceEvent::engine(EngineEvent::TurnComplete {
            turn_id: 1,
            history: None,
            meta: None,
        }));
        app.save_session_and_flush();

        let second_prompt_history_idx = app.session_snapshot().history.len();
        app.commit_request_history_item(
            HistoryItem::user(Content::text("second prompt")),
            Some(Block::User {
                text: "second prompt".into(),
                image_labels: Vec::new(),
                command: false,
                sent_at_ms: None,
            }),
        );
        app.start_turn(2);
        app.feed_one(SourceEvent::engine(EngineEvent::TokenUsage {
            usage: TokenUsage {
                context_tokens: Some(250),
                ..Default::default()
            },
            tokens_per_sec: None,
            cost_usd: None,
            background: false,
        }));

        app.rewind_to_history_index(Some(second_prompt_history_idx), false);
        app.save_session_and_flush();
        assert_eq!(app.session_message_count(), 2);
        assert_eq!(app.session_snapshot().display_context_tokens(), Some(100));
        app.session_snapshot().id.clone()
    };

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    let loaded = loaded_session(&resumed, &session_id);
    assert_eq!(loaded.history.len(), 2);
    assert_eq!(
        loaded.current_context_tokens(loaded.history.len()),
        Some(100)
    );
    assert_eq!(loaded.display_context_tokens(), Some(100));

    resumed.load_session_by_id(&session_id);
    assert_eq!(resumed.session_message_count(), 2);
    assert_eq!(
        resumed.session_snapshot().display_context_tokens(),
        Some(100)
    );
}

#[test]
fn cancel_or_shutdown_preserves_committed_tool_invocations() {
    enum Finalizer {
        Cancel,
        Shutdown,
    }

    let guard = test_home_guard();
    for finalizer in [Finalizer::Cancel, Finalizer::Shutdown] {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        app.start_turn(42);
        app.feed_one(SourceEvent::engine(
            protocol::EngineEvent::HistoryAppended {
                turn_id: 42,
                delta: protocol::CanonicalHistoryDelta::new(0, tool_history()),
            },
        ));

        match finalizer {
            Finalizer::Cancel => app.cancel(),
            Finalizer::Shutdown => {
                if app.agent_running() {
                    app.discard_turn(crate::app::TurnEnd::Cancelled);
                }
            }
        }
        app.save_session_and_flush();

        let loaded = loaded_session(&app, &app.session_snapshot().id);
        assert_committed_tool_invocation(&loaded.history);
    }
}

#[test]
fn store_backed_resume_restores_tool_calls_for_model_history() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        for item in tool_history() {
            app.session_append_history(item);
        }
        save_and_close_full_history_fixture(app)
    };

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    assert!(
        resumed.load_session_by_id(&session_id),
        "resume failed: {:?}",
        resumed
            .overlays_probe()
            .notification()
            .map(|notification| notification.summary.as_str())
    );

    assert_eq!(resumed.session_snapshot().id, session_id);
    assert!(
        resumed.tui.conversation.session().history.is_empty(),
        "resume should use the production sparse SQLite session path"
    );
    assert_eq!(resumed.session_message_count(), 2);

    let stored_history = resumed.session_history_range(0..resumed.session_message_count());
    assert_committed_tool_invocation(&stored_history);
    assert_model_history_tool_messages(&resumed.model_history_messages());
}

#[test]
fn store_backed_resume_then_continue_preserves_prior_tool_invocations() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        for item in tool_history() {
            app.session_append_history(item);
        }
        save_and_close_full_history_fixture(app)
    };

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.load_session_by_id(&session_id);
    resumed.session_append_history(HistoryItem::user(Content::text("continue after resume")));
    resumed.save_session_and_flush();

    let loaded = loaded_session(&resumed, &session_id);
    assert_eq!(loaded.history.len(), 3);
    assert_committed_tool_invocation(&loaded.history);
    assert!(matches!(
        loaded.history.last(),
        Some(HistoryItem::User { content, .. }) if content.text_content() == "continue after resume"
    ));
}

#[test]
fn repeated_store_backed_resume_cycles_preserve_all_history() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        for item in tool_history() {
            app.session_append_history(item);
        }
        save_and_close_full_history_fixture(app)
    };

    for cycle in 0..4 {
        let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
        assert!(
            resumed.load_session_by_id(&session_id),
            "cycle {cycle} resume failed: {:?}",
            resumed
                .overlays_probe()
                .notification()
                .map(|notification| notification.summary.as_str())
        );
        assert_eq!(resumed.session_snapshot().id, session_id);
        assert!(resumed.conversation_probe().has_live_session());
        resumed.session_append_history(HistoryItem::user(Content::text(format!("cycle {cycle}"))));
        resumed.save_session_and_flush();
        resumed
            .tui
            .shutdown_persist()
            .expect("close resumed fixture persistence");
    }

    let reader = TestApp::builder().build_without_test_home_reset(&guard);
    let loaded = loaded_session(&reader, &session_id);
    assert_eq!(loaded.history.len(), 6);
    assert_committed_tool_invocation(&loaded.history);
    for cycle in 0..4 {
        assert!(loaded
            .history
            .iter()
            .any(|item| { matches!(item, HistoryItem::User { content, .. } if content.text_content() == format!("cycle {cycle}")) }));
    }
}

fn wait_for_fork(app: &mut TestApp, source_id: &str) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while app.session_snapshot().id == source_id {
        app.tui.drain_persist_reports();
        if let Some(event) = app.try_recv_app_event() {
            app.handle_app_event(event);
        } else {
            assert!(
                std::time::Instant::now() < deadline,
                "fork did not complete: {:?}",
                app.overlays_probe()
                    .notification()
                    .map(|notice| &notice.summary)
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    app.wait_for_session_lifecycle();
    app.session_snapshot().id.clone()
}

#[test]
fn read_only_session_can_fork_while_its_owner_keeps_writing() {
    let guard = test_home_guard();
    let mut owner = TestApp::builder().build_with_test_home_guard(&guard);
    owner.session_append_history(HistoryItem::user(Content::text("shared history")));
    save_full_history_fixture(&mut owner);
    let source_id = owner.session_snapshot().id.clone();
    let mut reader = TestApp::builder().build_without_test_home_reset(&guard);
    assert!(reader.load_session_by_id(&source_id));
    assert!(reader.session_is_read_only());
    assert!(reader.run_lua("smelt.session.fork()"));
    let fork_id = wait_for_fork(&mut reader, &source_id);
    assert!(!reader.session_is_read_only());
    assert!(!owner.session_is_read_only());
    assert_eq!(
        session_reader(&fork_id).history_range(0, 1).unwrap(),
        vec![HistoryItem::user(Content::text("shared history"))]
    );
    owner.session_append_history(HistoryItem::user(Content::text("parent continues")));
    owner.save_session_and_flush();
    reader.session_append_history(HistoryItem::user(Content::text("fork continues")));
    reader.save_session_and_flush();
    assert_eq!(
        session_reader(&source_id).history_range(1, 2).unwrap(),
        vec![HistoryItem::user(Content::text("parent continues"))]
    );
    assert_eq!(
        session_reader(&fork_id).history_range(1, 2).unwrap(),
        vec![HistoryItem::user(Content::text("fork continues"))]
    );
}

#[test]
fn resume_recovery_contention_does_not_block_the_ui() {
    let guard = test_home_guard();
    let (source_id, turn_id, path) = {
        let mut owner = TestApp::builder().build_with_test_home_guard(&guard);
        owner.start_submitted_turn("interrupted request");
        owner.save_session_and_flush();
        let source_id = owner.session_snapshot().id.clone();
        let turn_id = owner.current_turn_id().unwrap();
        let path = owner.core_probe().sessions.store().database_path();
        (source_id, turn_id, path)
    };
    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    let blocker = rusqlite::Connection::open(path).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let elapsed = std::thread::scope(|scope| {
        scope.spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            blocker.execute_batch("ROLLBACK").unwrap();
        });
        let started = std::time::Instant::now();
        assert!(resumed.tui.load_session_by_id(&source_id));
        let elapsed = started.elapsed();
        assert!(resumed.tui.conversation.writer_is_opening());
        assert!(resumed.session_is_read_only());
        elapsed
    });
    assert!(
        elapsed < Duration::from_millis(200),
        "resume blocked the UI for {elapsed:?}"
    );
    resumed.wait_for_session_lifecycle();
    assert!(!resumed.session_is_read_only());
    assert_eq!(
        resumed.tui.conversation.last_terminal_turn_id(),
        Some(turn_id)
    );
}

#[test]
fn fork_storage_contention_does_not_block_the_ui() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    app.session_append_history(HistoryItem::user(Content::text("shared history")));
    save_full_history_fixture(&mut app);
    let source_id = app.session_snapshot().id.clone();
    let path = app.core_probe().sessions.store().database_path();
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    let elapsed = std::thread::scope(|scope| {
        scope.spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            conn.execute_batch("ROLLBACK").unwrap();
        });
        let start = std::time::Instant::now();
        assert!(app.run_lua("smelt.session.fork()"));
        start.elapsed()
    });
    assert!(
        elapsed < Duration::from_millis(200),
        "fork blocked the UI for {elapsed:?}"
    );
    wait_for_fork(&mut app, &source_id);
    assert!(!app.session_is_read_only());
}

#[test]
fn resuming_fork_while_parent_has_active_writer_is_writable() {
    let guard = test_home_guard();
    let mut parent = TestApp::builder().build_with_test_home_guard(&guard);
    parent.session_append_history(HistoryItem::user(Content::text("shared history")));
    save_full_history_fixture(&mut parent);
    let parent_id = parent.session_snapshot().id.clone();

    assert!(parent.run_lua("smelt.session.fork()"));
    let fork_id = wait_for_fork(&mut parent, &parent_id);
    assert_ne!(fork_id, parent_id);
    assert!(parent.load_session_by_id(&parent_id));
    assert!(!parent.session_is_read_only());

    let mut fork = TestApp::builder().build_without_test_home_reset(&guard);
    fork.set_lua_string_global("__fork_id", fork_id.clone())
        .unwrap();
    assert!(fork.run_lua("smelt.session.load(_G.__fork_id)"));
    wait_for_session_load(&mut fork, &fork_id);
    assert!(
        !fork.session_is_read_only(),
        "fork opened read-only while only its parent is owned: {:?}",
        fork.overlays_probe()
            .notification()
            .map(|notice| &notice.summary)
    );

    for (app, text) in [(&mut parent, "parent suffix"), (&mut fork, "fork suffix")] {
        app.commit_request_history_item(
            HistoryItem::user(Content::text(text)),
            Some(Block::User {
                text: text.into(),
                image_labels: Vec::new(),
                command: false,
                sent_at_ms: None,
            }),
        );
        app.save_session_and_flush();
        assert!(!app.session_is_read_only());
        assert!(!app.session_document_has_unflushed_work());
    }
    assert_eq!(
        session_reader(&parent_id).history_range(0, 2).unwrap(),
        vec![
            HistoryItem::user(Content::text("shared history")),
            HistoryItem::user(Content::text("parent suffix")),
        ]
    );
    assert_eq!(
        session_reader(&fork_id).history_range(0, 2).unwrap(),
        vec![
            HistoryItem::user(Content::text("shared history")),
            HistoryItem::user(Content::text("fork suffix")),
        ]
    );

    let mut reader = TestApp::builder().build_without_test_home_reset(&guard);
    assert!(
        reader.load_session_by_id(&fork_id),
        "{:?}",
        reader
            .overlays_probe()
            .notification()
            .map(|notice| &notice.summary)
    );
    assert!(reader.session_is_read_only());
    assert_eq!(
        reader.overlays_probe().notification().unwrap().kind,
        smelt_core::messages::MessageKind::Info,
        "a second window on the same fork must remain read-only"
    );
}

#[test]
fn resuming_session_with_active_writer_is_read_only() {
    let guard = test_home_guard();
    let mut writer = TestApp::builder().build_with_test_home_guard(&guard);
    writer.session_append_history(HistoryItem::user(Content::text("owned history")));
    save_full_history_fixture(&mut writer);

    let session_id = writer.session_snapshot().id.clone();
    let before = session_reader(&session_id).state().unwrap();
    let mut reader = TestApp::builder().build_without_test_home_reset(&guard);
    reader.load_session_by_id(&session_id);

    assert_eq!(reader.session_snapshot().id, session_id);
    assert!(reader.session_is_read_only());
    assert_eq!(reader.session_message_count(), 1);
    assert_eq!(
        reader.overlays_probe().notification().unwrap().kind,
        smelt_core::messages::MessageKind::Info,
        "opening a busy session read-only is informational"
    );
    assert_eq!(
        reader.overlays_probe().notification().unwrap().summary,
        "opened session read-only: this session is already open in another window"
    );
    assert!(reader
        .overlays_probe()
        .notification()
        .is_some_and(|notification| {
            notification.lifetime.is_sticky()
                && notification
                    .summary
                    .starts_with("opened session read-only:")
                && matches!(
                    &notification.scope,
                    crate::app::NotificationScope::Session(owner_session_id)
                        if owner_session_id == &session_id
                )
        }));

    let result = reader.apply_history_append_to_history(&HistoryAppend::append(HistoryItem::user(
        Content::text("read-only mutation"),
    )));
    assert_eq!(result, HistoryAppendResult::Unchanged);
    reader.save_session_and_flush();

    let after_reader = session_reader(&session_id);
    let after = after_reader.state().unwrap();
    assert_eq!(after.head, before.head);
    assert_eq!(
        after_reader
            .history_range(0, after.head.history_len.get())
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn read_only_session_becomes_writable_after_owner_closes() {
    let guard = test_home_guard();
    let mut owner = TestApp::builder().build_with_test_home_guard(&guard);
    owner.session_append_history(HistoryItem::user(Content::text("shared history")));
    save_full_history_fixture(&mut owner);
    let session_id = owner.session_snapshot().id.clone();
    let mut reader = TestApp::builder().build_without_test_home_reset(&guard);
    assert!(reader.load_session_by_id(&session_id));
    assert!(reader.session_is_read_only());
    reader.type_text("draft survives takeover");
    reader.press(crossterm::event::KeyCode::Enter);
    assert!(reader.session_is_read_only());
    assert_eq!(reader.prompt_source(), "draft survives takeover");

    owner.session_append_history(HistoryItem::user(Content::text("owner's last message")));
    owner.save_session_and_flush();
    drop(owner);

    wait_for_ownership_recovery(&mut reader);
    assert_eq!(reader.prompt_source(), "draft survives takeover");
    assert_eq!(reader.session_message_count(), 2);
    assert!(reader.overlays_probe().notification().is_none());
    reader.press(crossterm::event::KeyCode::Enter);
    reader.wait_for_turn_dispatch();
    assert!(reader.current_turn_id().is_some());
    assert!(!reader.session_is_read_only());
    reader.save_session_and_flush();
    let stored = session_reader(&session_id);
    let history = stored
        .history_range(0, stored.state().unwrap().head.history_len.get())
        .unwrap();
    assert_eq!(
        &history[..2],
        &[
            HistoryItem::user(Content::text("shared history")),
            HistoryItem::user(Content::text("owner's last message")),
        ]
    );
    assert!(matches!(
        history.last().unwrap(),
        HistoryItem::User { content, .. } if content.text_content() == "draft survives takeover"
    ));
}

fn wait_for_ownership_recovery(app: &mut TestApp) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while app.session_is_read_only() {
        app.tui.drain_persist_reports();
        if let Some(event) = app.tui.platform.try_recv_app_event() {
            app.handle_app_event(event);
        }
        assert!(
            std::time::Instant::now() < deadline,
            "ownership recovery timed out"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_for_ownership_event(
    observer: &std::sync::mpsc::Receiver<OwnershipRecoveryEvent>,
    expected: OwnershipRecoveryEvent,
) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let event = observer
            .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            .expect("ownership recovery event timed out");
        if event == expected {
            return;
        }
        assert_ne!(
            event,
            OwnershipRecoveryEvent::Stopped,
            "worker stopped before {expected:?}"
        );
    }
}

#[test]
fn failed_manual_load_does_not_cancel_ownership_recovery() {
    let guard = test_home_guard();
    let mut owner = TestApp::builder().build_with_test_home_guard(&guard);
    owner.session_append_history(HistoryItem::user(Content::text("shared history")));
    save_full_history_fixture(&mut owner);
    let session_id = owner.session_snapshot().id.clone();
    let mut reader = TestApp::builder().build_without_test_home_reset(&guard);
    assert!(reader.load_session_by_id(&session_id));
    let observer = reader.tui.conversation.take_ownership_observer();
    wait_for_ownership_event(&observer, OwnershipRecoveryEvent::Waiting);
    reader.tui.request_session_load("missing-session");
    reader.wait_for_session_lifecycle();
    assert_eq!(reader.session_snapshot().id, session_id);
    assert!(reader.session_is_read_only());
    drop(owner);
    wait_for_ownership_event(&observer, OwnershipRecoveryEvent::Ready);
    wait_for_ownership_recovery(&mut reader);
    assert_eq!(reader.session_snapshot().id, session_id);
}

#[test]
fn leaving_read_only_session_cancels_ownership_wait() {
    let guard = test_home_guard();
    let mut owner = TestApp::builder().build_with_test_home_guard(&guard);
    owner.session_append_history(HistoryItem::user(Content::text("shared history")));
    save_full_history_fixture(&mut owner);
    let mut reader = TestApp::builder().build_without_test_home_reset(&guard);
    assert!(reader.load_session_by_id(&owner.session_snapshot().id));
    assert!(reader.session_is_read_only());
    let observer = reader.tui.conversation.take_ownership_observer();
    wait_for_ownership_event(&observer, OwnershipRecoveryEvent::Waiting);
    reader.reset_session();
    let new_id = reader.session_snapshot().id.clone();
    reader.type_text("new session draft");
    wait_for_ownership_event(&observer, OwnershipRecoveryEvent::Stopped);
    drop(owner);

    reader.tui.drain_persist_reports();
    while let Some(event) = reader.tui.platform.try_recv_app_event() {
        reader.handle_app_event(event);
    }
    assert_eq!(reader.session_snapshot().id, new_id);
    assert_eq!(reader.prompt_source(), "new session draft");
    assert!(!reader.session_is_read_only());
}

#[test]
fn ownership_recovery_keeps_lease_until_writer_startup() {
    let guard = test_home_guard();
    let mut owner = TestApp::builder().build_with_test_home_guard(&guard);
    owner.session_append_history(HistoryItem::user(Content::text("shared history")));
    save_full_history_fixture(&mut owner);
    let session_id = owner.session_snapshot().id.clone();
    let mut reader = TestApp::builder().build_without_test_home_reset(&guard);
    assert!(reader.load_session_by_id(&session_id));
    let observer = reader.tui.conversation.take_ownership_observer();
    wait_for_ownership_event(&observer, OwnershipRecoveryEvent::Waiting);
    drop(owner);
    wait_for_ownership_event(&observer, OwnershipRecoveryEvent::Ready);

    let sessions = smelt_core::session::storage();
    let store = sessions.store();
    assert!(reader.session_is_read_only());
    assert!(matches!(
        store.writer(&session_id),
        Err(smelt_store::StoreError::Owned { .. })
    ));
    wait_for_ownership_recovery(&mut reader);
    assert!(matches!(
        store.writer(&session_id),
        Err(smelt_store::StoreError::Owned { .. })
    ));
    reader.start_submitted_turn("continue after handoff");
    assert!(reader.current_turn_id().is_some());
}

#[test]
fn successful_manual_load_discards_ready_ownership_recovery() {
    let guard = test_home_guard();
    let target_id = saved_one_row_session(&guard);
    let mut owner = TestApp::builder().build_without_test_home_reset(&guard);
    owner.session_append_history(HistoryItem::user(Content::text("shared history")));
    save_full_history_fixture(&mut owner);
    let source_id = owner.session_snapshot().id.clone();
    let mut reader = TestApp::builder().build_without_test_home_reset(&guard);
    assert!(reader.load_session_by_id(&source_id));
    let observer = reader.tui.conversation.take_ownership_observer();
    wait_for_ownership_event(&observer, OwnershipRecoveryEvent::Waiting);
    drop(owner);
    wait_for_ownership_event(&observer, OwnershipRecoveryEvent::Ready);

    reader.tui.request_session_load(&target_id);
    reader.wait_for_session_lifecycle();
    assert_eq!(reader.session_snapshot().id, target_id);
    assert!(!reader.session_is_read_only());
    wait_for_ownership_event(&observer, OwnershipRecoveryEvent::Stopped);
    let _released = smelt_core::session::storage()
        .store()
        .writer(&source_id)
        .expect("leaving a session must release its recovered writer");
}

#[test]
fn competing_readers_recover_latest_history_in_sequence() {
    let guard = test_home_guard();
    let mut owner = TestApp::builder().build_with_test_home_guard(&guard);
    owner.session_append_history(HistoryItem::user(Content::text("shared history")));
    save_full_history_fixture(&mut owner);
    let session_id = owner.session_snapshot().id.clone();
    let mut first = TestApp::builder().build_without_test_home_reset(&guard);
    let mut second = TestApp::builder().build_without_test_home_reset(&guard);
    for reader in [&mut first, &mut second] {
        assert!(reader.load_session_by_id(&session_id));
        let observer = reader.tui.conversation.take_ownership_observer();
        wait_for_ownership_event(&observer, OwnershipRecoveryEvent::Waiting);
    }
    drop(owner);

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while first.session_is_read_only() && second.session_is_read_only() {
        for reader in [&mut first, &mut second] {
            reader.tui.drain_persist_reports();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no reader acquired ownership"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let (winner, waiter) = if first.session_is_read_only() {
        (&mut second, &mut first)
    } else {
        (&mut first, &mut second)
    };
    assert!(waiter.session_is_read_only());
    winner.session_append_history(HistoryItem::user(Content::text("winner continues")));
    winner.save_session_and_flush();
    winner.reset_session();
    wait_for_ownership_recovery(waiter);
    assert_eq!(waiter.session_message_count(), 2);
    assert_eq!(
        session_reader(&session_id).history_range(0, 2).unwrap(),
        vec![
            HistoryItem::user(Content::text("shared history")),
            HistoryItem::user(Content::text("winner continues")),
        ]
    );
}

#[test]
fn ownership_recovery_loads_interrupted_turn_before_writer_startup() {
    let guard = test_home_guard();
    let mut owner = TestApp::builder().build_with_test_home_guard(&guard);
    owner.start_submitted_turn("interrupted request");
    owner.save_session_and_flush();
    let session_id = owner.session_snapshot().id.clone();
    let interrupted_turn = owner.current_turn_id().unwrap();
    let mut reader = TestApp::builder().build_without_test_home_reset(&guard);
    assert!(reader.load_session_by_id(&session_id));
    assert!(reader.session_is_read_only());
    drop(owner);
    wait_for_ownership_recovery(&mut reader);
    assert_eq!(
        reader.tui.conversation.last_terminal_turn_id(),
        Some(interrupted_turn)
    );
    reader.start_submitted_turn("continue after interruption");
    assert!(reader
        .current_turn_id()
        .is_some_and(|turn| turn > interrupted_turn));
}

#[test]
fn resuming_session_with_writer_io_failure_still_reports_an_error() {
    let guard = test_home_guard();
    let session_id = saved_one_row_session(&guard);
    let lock_path = smelt_core::session::storage()
        .store()
        .locks_dir()
        .join(format!("{session_id}.lock"));
    let _ = std::fs::remove_file(&lock_path);
    std::fs::create_dir_all(&lock_path).unwrap();

    let mut reader = TestApp::builder().build_without_test_home_reset(&guard);
    assert!(reader.load_session_by_id(&session_id));
    assert!(reader.session_is_read_only());
    assert_eq!(reader.session_message_count(), 1);
    let overlays = reader.overlays_probe();
    let notice = overlays.notification().unwrap();
    assert_eq!(notice.kind, smelt_core::messages::MessageKind::Error);
    assert!(notice
        .summary
        .starts_with("opened session read-only: open session:"));
}

#[test]
fn loading_writable_session_clears_prior_ownership_conflict_notification() {
    let guard = test_home_guard();
    let writable_session_id = saved_one_row_session(&guard);
    let mut owner = TestApp::builder().build_without_test_home_reset(&guard);
    owner.session_append_history(HistoryItem::user(Content::text("owned history")));
    save_full_history_fixture(&mut owner);
    let owned_session_id = owner.session_snapshot().id.clone();
    let mut reader = TestApp::builder().build_without_test_home_reset(&guard);

    reader
        .set_lua_string_global("__owned_session_id", owned_session_id.clone())
        .unwrap();
    assert!(reader.run_lua("smelt.session.load(_G.__owned_session_id)"));
    wait_for_session_load(&mut reader, &owned_session_id);
    assert!(reader
        .overlays_probe()
        .notification()
        .is_some_and(|notification| {
            notification
                .summary
                .starts_with("opened session read-only:")
        }));
    assert!(reader.lua_messages_contain("opened session read-only:"));

    reader
        .set_lua_string_global("__writable_session_id", writable_session_id.clone())
        .unwrap();
    assert!(reader.run_lua("smelt.session.load(_G.__writable_session_id)"));
    wait_for_session_load(&mut reader, &writable_session_id);

    assert_eq!(reader.session_snapshot().id, writable_session_id);
    assert!(!reader.session_is_read_only());
    assert!(reader.overlays_probe().notification().is_none());
    assert!(reader.lua_messages_contain("opened session read-only:"));
}

#[test]
fn successful_session_load_clears_load_failure_notification() {
    let guard = test_home_guard();
    let session_id = saved_one_row_session(&guard);
    let mut app = TestApp::builder().build_without_test_home_reset(&guard);

    app.load_session_by_id("missing-session");
    assert!(app
        .overlays_probe()
        .notification()
        .is_some_and(|notification| {
            matches!(
                &notification.scope,
                crate::app::NotificationScope::Operation(
                    crate::app::NotificationOperation::SessionLoad
                )
            )
        }));

    app.load_session_by_id(&session_id);

    assert_eq!(app.session_snapshot().id, session_id);
    assert!(app.overlays_probe().notification().is_none());
}

#[test]
fn successful_session_load_preserves_workspace_notification() {
    let guard = test_home_guard();
    let session_id = saved_one_row_session(&guard);
    let mut app = TestApp::builder().build_without_test_home_reset(&guard);
    app.notify_workspace_error_sticky("workspace failure".into());

    app.load_session_by_id(&session_id);

    assert_eq!(app.session_snapshot().id, session_id);
    assert!(app
        .overlays_probe()
        .notification()
        .is_some_and(|notification| {
            matches!(
                &notification.scope,
                crate::app::NotificationScope::Workspace
            ) && notification.summary == "workspace failure"
        }));
}

#[test]
fn session_switch_dismisses_suspended_ownership_conflict_notification() {
    let guard = test_home_guard();
    let writable_session_id = saved_one_row_session(&guard);
    let mut owner = TestApp::builder().build_without_test_home_reset(&guard);
    owner.session_append_history(HistoryItem::user(Content::text("owned history")));
    save_full_history_fixture(&mut owner);
    let owned_session_id = owner.session_snapshot().id.clone();
    let mut reader = TestApp::builder().build_without_test_home_reset(&guard);
    reader.load_session_by_id(&owned_session_id);
    assert!(reader.run_lua(
        r#"
        local leaf = smelt.dialog.content({ text = "Review this action" })
        smelt.dialog.new({
          panels = { { leaf = leaf, height = "fit" } },
          blocks_agent = true,
        })
        "#,
    ));
    assert!(reader
        .overlays_probe()
        .suspended_notification()
        .is_some_and(|notification| {
            matches!(
                &notification.scope,
                crate::app::NotificationScope::Session(owner_session_id)
                    if owner_session_id == &owned_session_id
            )
        }));

    reader.load_session_by_id(&writable_session_id);

    assert_eq!(reader.session_snapshot().id, writable_session_id);
    assert!(!reader.session_is_read_only());
    assert!(reader.overlays_probe().notification().is_none());
    assert!(reader.overlays_probe().suspended_notification().is_none());
}

#[test]
fn loading_another_owned_session_replaces_warning_with_target_session_scope() {
    let guard = test_home_guard();
    let mut first_owner = TestApp::builder().build_with_test_home_guard(&guard);
    first_owner.session_append_history(HistoryItem::user(Content::text("first owned history")));
    save_full_history_fixture(&mut first_owner);
    let first_id = first_owner.session_snapshot().id.clone();
    let mut second_owner = TestApp::builder().build_without_test_home_reset(&guard);
    second_owner.session_append_history(HistoryItem::user(Content::text("second owned history")));
    save_full_history_fixture(&mut second_owner);
    let second_id = second_owner.session_snapshot().id.clone();
    let mut reader = TestApp::builder().build_without_test_home_reset(&guard);

    reader.load_session_by_id(&first_id);
    reader.load_session_by_id(&second_id);

    assert_eq!(reader.session_snapshot().id, second_id);
    assert!(reader.session_is_read_only());
    assert!(reader
        .overlays_probe()
        .notification()
        .is_some_and(|notification| {
            notification
                .summary
                .starts_with("opened session read-only:")
                && matches!(
                    &notification.scope,
                    crate::app::NotificationScope::Session(owner_session_id)
                        if owner_session_id == &second_id
                )
        }));
}

#[test]
fn ownership_loss_moves_session_to_read_only_and_keeps_document_dirty() {
    let guard = test_home_guard();
    let session_id = saved_one_row_session(&guard);
    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    assert!(
        resumed.load_session_by_id(&session_id),
        "resume failed: {:?}",
        resumed
            .overlays_probe()
            .notification()
            .map(|notification| notification.summary.as_str())
    );
    assert!(!resumed.session_is_read_only());
    resumed.inject_commit_failure(smelt_store::StoreError::Owned {
        session_id: String::new(),
    });
    resumed.session_append_history(HistoryItem::user(Content::text(
        "unsaved after ownership loss",
    )));

    resumed.save_session_and_flush();

    assert!(resumed.session_is_read_only());
    assert!(resumed.session_document_has_unflushed_work());
    assert!(has_sticky_session_save_failure(&resumed, &session_id));
}

#[test]
fn ownership_loss_with_dirty_state_can_fork_to_a_writable_session() {
    let guard = test_home_guard();
    let session_id = saved_one_row_session(&guard);
    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    assert!(
        resumed.load_session_by_id(&session_id),
        "resume failed: {:?}",
        resumed
            .overlays_probe()
            .notification()
            .map(|notification| notification.summary.as_str())
    );
    resumed.start_turn(42);
    resumed.feed_one(SourceEvent::engine(EngineEvent::TextDelta {
        delta: "stream finalized by fork cancellation".into(),
    }));
    assert!(resumed.streaming_state().text);
    resumed.inject_commit_failure(smelt_store::StoreError::Owned {
        session_id: String::new(),
    });
    resumed.session_append_history(HistoryItem::user(Content::text(
        "preserved in ownership-loss fork",
    )));
    resumed.push_transcript_block(Block::Text {
        content: "unsaved ownership-loss record".into(),
    });
    resumed.save_session_and_flush();
    assert!(resumed.session_is_read_only());
    assert!(resumed.session_document_has_unflushed_work());

    resumed.fork_session();

    let fork_id = resumed.session_snapshot().id.clone();
    assert_ne!(fork_id, session_id);
    assert!(!resumed.session_is_read_only());
    assert!(!resumed.session_document_has_unflushed_work());
    assert!(resumed.tui.conversation.session().history.is_empty());
    assert!(resumed.conversation_probe().has_live_session());
    let reader = session_reader(&fork_id);
    let state = reader.state().unwrap();
    let record_rows = reader
        .transcript_range(0, state.head.transcript_record_count.get())
        .unwrap();
    assert!(record_rows
        .iter()
        .any(|row| row.preview_text().contains("unsaved ownership-loss record")));
    assert!(state.turn_metas.iter().any(|(history_len, meta)| {
        *history_len == 2 && meta["interrupted"] == serde_json::Value::Bool(true)
    }));
    let forked = loaded_session(&resumed, &fork_id);
    assert_eq!(forked.parent_id.as_deref(), Some(session_id.as_str()));
    assert_eq!(forked.history.len(), 2);
    assert!(matches!(
        forked.history.last(),
        Some(HistoryItem::User { content, .. })
            if content.text_content() == "preserved in ownership-loss fork"
    ));
    assert_eq!(loaded_session(&resumed, &session_id).history.len(), 1);
}

#[test]
fn saved_new_session_keeps_only_model_context_in_memory() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    for idx in 0..4 {
        app.session_append_history(HistoryItem::user(Content::text(format!("row {idx}"))));
    }
    app.session_set_checkpoint(Some(smelt_core::ContextCheckpoint {
        kind: "compaction".into(),
        summary: "summary".into(),
        first_live_index: 3,
        created_at_ms: 1,
        tokens_before: None,
        tokens_after_estimate: None,
        tokens_after_estimate_history_len: None,
        pre_checkpoint_context_tokens: None,
        pre_checkpoint_context_history_len: None,
    }));
    app.save_session_and_flush();

    assert!(app.tui.conversation.session().history.is_empty());
    let live = app
        .conversation_probe()
        .live_session()
        .expect("saved session reads history from the store");
    assert_eq!((live.live_start, live.history_len()), (3, 4));
    let rows: Vec<_> = app
        .session_history()
        .into_iter()
        .filter_map(|item| match item {
            HistoryItem::User { content, .. } => Some(content.text_content().to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(rows, ["row 0", "row 1", "row 2", "row 3"]);
}

#[test]
fn live_save_restarts_at_stored_prefix_when_dirty_marker_skips_missing_row() {
    let guard = test_home_guard();
    let session_id = saved_one_row_session(&guard);

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.load_session_by_id(&session_id);
    let stored_len = resumed.session_message_count();
    assert_eq!(stored_len, 1);

    resumed.session_append_history(HistoryItem::user(Content::text("kept live row")));
    assert!(resumed.conversation_probe().has_live_session());
    resumed.set_history_resave_from_for_harness(stored_len + 1);

    resumed.save_session();
    resumed.flush_persist();

    assert!(
        resumed.overlays_probe().notification().is_none(),
        "save should not surface a prefix-exceeds-stored integrity error"
    );
    let live = resumed
        .conversation_probe()
        .live_session()
        .expect("store-backed session");
    // Without a checkpoint the whole history is model context, so it stays in memory.
    assert_eq!(live.live_suffix_len(), live.history_len());

    let loaded = loaded_session(&resumed, &session_id);
    assert_eq!(loaded.history.len(), stored_len + 1);
    assert!(matches!(
        loaded.history.last(),
        Some(HistoryItem::User { content, .. }) if content.text_content() == "kept live row"
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn pre_request_compaction_append_save_resume_keeps_canonical_history() {
    fn compacted_marker_count(app: &TestApp) -> usize {
        let history = app.conversation_probe().transcript().history();
        (0..history.len())
            .filter(|index| {
                history
                    .block_id_at(*index)
                    .and_then(|id| history.block_kind(id))
                    == Some("compacted")
            })
            .count()
    }

    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    for idx in 0..24 {
        app.session_append_history(HistoryItem::user(Content::text(format!("row {idx}"))));
    }
    let compacted_prefix_len = app.session_message_count();
    app.start_turn(1);
    app.feed_one(SourceEvent::engine(EngineEvent::TokenUsage {
        usage: TokenUsage {
            context_tokens: Some(100),
            ..Default::default()
        },
        tokens_per_sec: None,
        cost_usd: None,
        background: false,
    }));
    app.feed_one(SourceEvent::engine(EngineEvent::TurnComplete {
        turn_id: 1,
        history: None,
        meta: None,
    }));
    app.save_session_and_flush();

    let mut settings = app.core_probe().config.settings.clone();
    settings.auto_compact = true;
    settings.compact_threshold = 0.8;
    settings.compact_keep_recent_groups = 1.0;
    app.set_settings_for_harness(settings);
    app.set_context_window(Some(100));
    app.commit_request_history_item(
        HistoryItem::user(Content::text("request after compaction")),
        Some(Block::User {
            text: "request after compaction".into(),
            image_labels: Vec::new(),
            command: false,
            sent_at_ms: None,
        }),
    );
    app.start_turn(42);

    let messages = protocol::history_to_messages(&app.model_history());
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    {
        app.dispatch_host_call(engine::HostCall::PrepareRequest {
            turn_id: app.current_turn_id().expect("active request turn"),
            messages: engine::PreparedRequestMessages::model_only(messages),
            estimated_tokens: 200,
            reply: tx,
        });
    }
    let ask_id = app
        .drain_engine_sends()
        .into_iter()
        .filter_map(|command| match command {
            protocol::UiCommand::EngineAsk { id, .. } => Some(id),
            _ => None,
        })
        .next_back()
        .expect("pre-request compaction should issue EngineAsk");
    {
        app.dispatch_engine_event(EngineEvent::EngineAskResponse {
            id: ask_id,
            message: Some(protocol::Message::assistant(
                Some(Content::text("# Goal\nretained summary")),
                None,
                None,
            )),
            error: None,
        });
        app.drive_lua_tasks();
    }
    assert_eq!(compacted_marker_count(&app), 1);

    let (replacement, coordinates) = match rx
        .try_recv()
        .expect("compaction prepare reply should be ready")
    {
        engine::HostRequestDecision::ReplaceModelHistory {
            history,
            coordinates,
        } => (history, coordinates),
        decision => panic!("expected model-history replacement, got {decision:?}"),
    };
    assert_eq!(coordinates.model_prefix_len(), 1);
    assert_eq!(coordinates.canonical_start().get(), compacted_prefix_len);
    assert_eq!(replacement.len(), 2);

    app.feed_one(SourceEvent::engine(EngineEvent::HistoryUpdated {
        turn_id: 42,
        update: coordinates.canonical_delta(protocol::ModelHistoryIndex::ZERO, replacement),
    }));
    assert_eq!(compacted_marker_count(&app), 1);
    app.feed_one(SourceEvent::engine(EngineEvent::HistoryAppended {
        turn_id: 42,
        delta: protocol::CanonicalHistoryDelta {
            first_index: coordinates.canonical_index(protocol::ModelHistoryIndex::new(2)),
            items: vec![HistoryItem::Assistant(AssistantStep::terminal(
                Some(Content::text("reply after compaction")),
                None,
                Vec::new(),
            ))],
        },
    }));
    app.save_session_and_flush();

    assert!(
        app.overlays_probe().notification().is_none(),
        "compacted append should save without a side-table bounds error: {:?}",
        app.overlays_probe().notification()
    );
    assert_eq!(app.session_message_count(), compacted_prefix_len + 2);
    let session_id = app.session_snapshot().id.clone();
    let loaded = loaded_session(&app, &session_id);
    assert_eq!(loaded.history.len(), compacted_prefix_len + 2);
    assert!(loaded
        .context_snapshots
        .iter()
        .any(|(index, _)| *index == compacted_prefix_len));
    assert!(loaded
        .context_snapshots
        .iter()
        .all(|(index, _)| *index <= loaded.history.len()));
    assert!(matches!(
        loaded.history.first(),
        Some(HistoryItem::User { content, .. }) if content.text_content() == "row 0"
    ));
    assert!(matches!(
        loaded.history.get(compacted_prefix_len),
        Some(HistoryItem::User { content, .. }) if content.text_content() == "request after compaction"
    ));
    assert!(matches!(
        loaded.history.last(),
        Some(HistoryItem::Assistant(step))
            if step.content.as_ref().is_some_and(|content| content.text_content() == "reply after compaction")
    ));

    drop(app);
    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.load_session_by_id(&session_id);
    assert_eq!(compacted_marker_count(&resumed), 1);
    assert_eq!(resumed.session_message_count(), compacted_prefix_len + 2);
    let resumed_history = resumed.session_history_range(0..resumed.session_message_count());
    assert!(matches!(
        resumed_history.first(),
        Some(HistoryItem::User { content, .. }) if content.text_content() == "row 0"
    ));
    assert!(matches!(
        resumed_history.last(),
        Some(HistoryItem::Assistant(step))
            if step.content.as_ref().is_some_and(|content| content.text_content() == "reply after compaction")
    ));
}

#[test]
fn sparse_resume_compaction_keeps_completed_marker_at_tail() {
    const EXCHANGE_COUNT: usize = 300;
    const HISTORY_LEN: usize = EXCHANGE_COUNT * 2 + 1;
    const RETAINED_HISTORY_INDEX: usize = HISTORY_LEN - 1;
    const SUMMARY: &str = "sparse retained checkpoint summary";

    fn compacted_marker_index(app: &TestApp) -> Option<usize> {
        let history = app.conversation_probe().transcript().history();
        (0..history.len()).find(|index| {
            history
                .block_id_at(*index)
                .and_then(|id| history.block_kind(id))
                == Some("compacted")
        })
    }

    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        for index in 0..EXCHANGE_COUNT {
            app.commit_request_history_item(
                HistoryItem::user(Content::text(format!("user {index}"))),
                Some(Block::User {
                    text: format!("user {index}"),
                    image_labels: Vec::new(),
                    command: false,
                    sent_at_ms: None,
                }),
            );
            app.session_append_history(HistoryItem::assistant(AssistantStep::terminal(
                Some(Content::text(format!("assistant {index}"))),
                None,
                Vec::new(),
            )));
            // Streaming assistant blocks are intentionally not linked to a canonical
            // history row.
            app.push_transcript_block(Block::Text {
                content: format!("assistant {index}").into(),
            });
        }
        // Tool turns commonly produce consecutive assistant history items. The
        // retained terminal response must remain after the preceding unoriginated
        // assistant or tool blocks rather than after the last originated user block.
        app.session_append_history(HistoryItem::assistant(AssistantStep::terminal(
            Some(Content::text("retained terminal assistant")),
            None,
            Vec::new(),
        )));
        app.push_transcript_block(Block::Text {
            content: "retained terminal assistant".into(),
        });
        app.save_session_and_flush();
        app.session_snapshot().id.clone()
    };

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.load_session_by_id(&session_id);
    assert!(resumed.tui.conversation.session().history.is_empty());
    let initial_loaded_len = resumed.conversation_probe().transcript().history().len();
    assert!(
        initial_loaded_len < HISTORY_LEN,
        "test requires a bounded sparse transcript tail"
    );
    assert!(resumed.run_lua(&format!(
        r#"assert(smelt.session.checkpoint({{
            summary = {SUMMARY:?},
            first_live_message_index = {RETAINED_HISTORY_INDEX},
            tokens_before = 100,
        }}))"#
    )));

    let marker_index = compacted_marker_index(&resumed).expect("live compacted marker");
    assert_eq!(
        marker_index + 1,
        resumed.conversation_probe().transcript().history().len(),
        "completed marker should remain at the transcript tail"
    );
    resumed.save_session_and_flush();

    let reader = session_reader(&session_id);
    let state = reader.state().unwrap();
    let durable_marker = reader
        .transcript_range(0, state.head.transcript_record_count.get())
        .unwrap()
        .into_iter()
        .find(|record| record.kind == "compacted")
        .expect("compacted marker persisted as a transcript record");
    let origin: smelt_core::BlockOrigin =
        serde_json::from_str(durable_marker.origin_json.as_deref().unwrap()).unwrap();
    assert_eq!(
        origin,
        smelt_core::BlockOrigin::Checkpoint {
            history_index: HISTORY_LEN,
        }
    );
    drop(resumed);

    let mut reloaded = TestApp::builder().build_without_test_home_reset(&guard);
    reloaded.load_session_by_id(&session_id);
    let marker_index = compacted_marker_index(&reloaded).expect("resumed compacted marker");
    assert_eq!(
        marker_index + 1,
        reloaded.conversation_probe().transcript().history().len(),
        "resumed marker should remain at its completion position"
    );
    assert!(reloaded.run_lua("smelt.transcript.fold_kind('compacted', 'open')"));
    assert!(
        reloaded.render_to_frame().text().contains(SUMMARY),
        "resumed compacted marker should remain expandable"
    );
}

#[test]
fn live_rewind_below_checkpoint_then_next_append_saves_without_bad_checkpoint() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        for idx in 0..4 {
            let text = format!("row {idx}");
            app.commit_request_history_item(
                HistoryItem::user(Content::text(text.clone())),
                Some(Block::User {
                    text,
                    image_labels: Vec::new(),
                    command: false,
                    sent_at_ms: None,
                }),
            );
        }
        app.session_set_checkpoint(Some(smelt_core::ContextCheckpoint {
            kind: "compaction".into(),
            summary: "retained summary".into(),
            first_live_index: 3,
            created_at_ms: 1,
            tokens_before: None,
            tokens_after_estimate: None,
            tokens_after_estimate_history_len: None,
            pre_checkpoint_context_tokens: None,
            pre_checkpoint_context_history_len: None,
        }));
        app.save_session_and_flush();
        app.session_snapshot().id.clone()
    };

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.load_session_by_id(&session_id);
    assert_eq!(
        resumed
            .conversation_probe()
            .session()
            .checkpoint
            .as_ref()
            .map(|checkpoint| checkpoint.first_live_index),
        Some(3)
    );
    let identity = resumed.active_context_token_identity();
    resumed.rewind_history_for_harness(1, false, identity);
    assert!(resumed.session_snapshot().checkpoint.is_none());

    resumed.session_append_history(HistoryItem::user(Content::text("new after rewind")));
    resumed.save_session();
    let hydration_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while resumed.tui.pending_session_save {
        assert!(
            std::time::Instant::now() < hydration_deadline,
            "transcript record hydration did not complete"
        );
        resumed.render_silent();
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(matches!(
        resumed.flush_persist(),
        crate::persist::PersistenceFlushOutcome::Durable
    ));

    assert!(
        resumed.overlays_probe().notification().is_none(),
        "save after rewind should not surface a checkpoint/history integrity error: {:?}",
        resumed.overlays_probe().notification()
    );
}

#[test]
fn in_flight_live_save_then_rewind_flushes_without_bad_prefix() {
    let guard = test_home_guard();
    let session_id = saved_one_row_session(&guard);

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.load_session_by_id(&session_id);
    resumed.session_append_history(HistoryItem::user(Content::text("save before rewind")));
    resumed.save_session();
    assert!(resumed.session_document_has_unflushed_work());

    resumed.rewind_to_start();
    resumed.save_session_and_flush();

    assert!(
        resumed.overlays_probe().notification().is_none(),
        "rewind after an in-flight live save should not surface a save error"
    );
    let loaded = loaded_session(&resumed, &session_id);
    assert!(loaded.history.is_empty());
}

#[test]
fn repeated_read_only_resumes_do_not_modify_writer_session() {
    let guard = test_home_guard();
    let mut writer = TestApp::builder().build_with_test_home_guard(&guard);
    writer.session_append_history(HistoryItem::user(Content::text("writer row")));
    save_full_history_fixture(&mut writer);

    let session_id = writer.session_snapshot().id.clone();
    let before = session_reader(&session_id).state().unwrap();
    for idx in 0..5 {
        let mut reader = TestApp::builder().build_without_test_home_reset(&guard);
        reader.load_session_by_id(&session_id);
        assert!(
            reader.session_is_read_only(),
            "reader {idx} should be read-only"
        );
        assert_eq!(reader.session_message_count(), 1);
        let result = reader.apply_history_append_to_history(&HistoryAppend::append(
            HistoryItem::user(Content::text(format!("ignored reader row {idx}"))),
        ));
        assert_eq!(result, HistoryAppendResult::Unchanged);
        reader.save_session_and_flush();
    }

    let after_reader = session_reader(&session_id);
    let after = after_reader.state().unwrap();
    assert_eq!(after.head, before.head);
    assert_eq!(
        after_reader
            .history_range(0, after.head.history_len.get())
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test(flavor = "current_thread")]
async fn canonical_history_rebuild_under_memory_budget_saves_and_rehydrates() {
    const ROWS: usize = 24;
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);
    for idx in 0..ROWS {
        let text = format!("row {idx}");
        app.commit_request_history_item(
            HistoryItem::user(Content::text(text.clone())),
            Some(Block::User {
                text,
                image_labels: Vec::new(),
                command: false,
                sent_at_ms: None,
            }),
        );
    }
    app.save_session_and_flush();
    let session_id = app.session_snapshot().id.clone();
    app.set_transcript_memory_budget_for_harness(crate::app::transcript::TranscriptMemoryBudget {
        hydrated_blocks: 1,
        ..Default::default()
    });

    // The engine rewrites the first item with a different kind, so the
    // transcript no longer links to history and rebuilds from scratch.
    let mut replacement = (0..ROWS)
        .map(|idx| HistoryItem::user(Content::text(format!("row {idx}"))))
        .collect::<Vec<_>>();
    replacement[0] = HistoryItem::Assistant(AssistantStep::terminal(
        Some(Content::text("rewritten reply")),
        None,
        Vec::new(),
    ));
    app.start_turn(7);
    app.feed_one(SourceEvent::engine(EngineEvent::HistoryUpdated {
        turn_id: 7,
        update: protocol::CanonicalHistoryDelta::new(0, replacement),
    }));
    app.feed_one(SourceEvent::engine(EngineEvent::TurnComplete {
        turn_id: 7,
        history: None,
        meta: None,
    }));
    app.render_silent();
    app.save_session_and_flush();

    assert!(
        !has_sticky_session_save_failure(&app, &session_id),
        "rebuilt transcript failed to save: {:?}",
        app.overlays_probe().notification()
    );
    let order = app
        .conversation_probe()
        .transcript()
        .history()
        .order
        .clone();
    assert_eq!(order.len(), ROWS);
    for id in &order {
        assert!(
            app.hydrate_transcript_blocks(&[*id]),
            "rebuilt block {id:?} cannot be hydrated"
        );
    }
    let loaded = loaded_session(&app, &session_id);
    assert!(matches!(
        loaded.history.first(),
        Some(HistoryItem::Assistant(step))
            if step.content.as_ref().is_some_and(|content| content.text_content() == "rewritten reply")
    ));
}
