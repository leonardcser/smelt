use super::*;
use protocol::{
    AssistantStep, Content, HistoryAppend, HistoryAppendResult, HistoryItem, Role, ToolInvocation,
    ToolOutcome,
};
use std::time::{SystemTime, UNIX_EPOCH};

fn loaded_session(id: &str) -> smelt_core::session::Session {
    crate::app::history::materialize_full_session(
        id,
        crate::app::history::FullSessionMaterializationReason::TestSavedSessionAssertion,
    )
    .expect("session saved")
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
                result: ToolOutcome {
                    content: "persisted\n".into(),
                    is_error: false,
                    metadata: None,
                },
                elapsed_ms: Some(12),
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
fn shutdown_flushes_latest_generation_after_in_flight_save() {
    let guard = test_home_guard();
    let mut app = TestApp::builder().build_with_test_home_guard(&guard);

    app.app
        .session_append_history(HistoryItem::user(Content::text("first generation")));
    app.app.save_session();
    assert!(app.app.session_persist.pending_save.is_some());

    app.app
        .session_append_history(HistoryItem::user(Content::text("final generation")));
    app.app.save_session();
    assert!(app.app.session_persist.save_pending);

    app.app.save_session_and_flush();

    let loaded = loaded_session(&app.app.core.session.id);
    assert_eq!(loaded.history.len(), 2);
    assert!(matches!(
        loaded.history.last(),
        Some(HistoryItem::User { content, .. }) if content.text_content() == "final generation"
    ));
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
        app.feed_one(SourceEvent::Engine(
            protocol::EngineEvent::HistoryAppended {
                turn_id: 42,
                first_index: 0,
                items: tool_history(),
            },
        ));

        match finalizer {
            Finalizer::Cancel => app.cancel(),
            Finalizer::Shutdown => {
                if app.app.agent.is_some() {
                    app.app.finish_turn(crate::app::TurnEnd::Cancelled);
                }
            }
        }
        app.app.save_session_and_flush();

        let loaded = loaded_session(&app.app.core.session.id);
        assert_committed_tool_invocation(&loaded.history);
    }
}

#[test]
fn store_backed_resume_restores_tool_calls_for_model_history() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        for item in tool_history() {
            app.app.session_append_history(item);
        }
        app.app.save_session_and_flush();
        app.app.core.session.id.clone()
    };

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.app.load_session_by_id(&session_id);

    assert_eq!(resumed.app.core.session.id, session_id);
    assert!(
        resumed.app.core.session.history.is_empty(),
        "resume should use the production sparse SQLite session path"
    );
    assert_eq!(resumed.app.session_history_len(), 2);

    let stored_history = resumed
        .app
        .session_history_range(0..resumed.app.session_history_len());
    assert_committed_tool_invocation(&stored_history);
    assert_model_history_tool_messages(&resumed.app.model_history_messages());
}

#[test]
fn store_backed_resume_then_continue_preserves_prior_tool_invocations() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        for item in tool_history() {
            app.app.session_append_history(item);
        }
        app.app.save_session_and_flush();
        app.app.core.session.id.clone()
    };

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.app.load_session_by_id(&session_id);
    resumed
        .app
        .session_append_history(HistoryItem::user(Content::text("continue after resume")));
    resumed.app.save_session_and_flush();

    let loaded = loaded_session(&session_id);
    assert_eq!(loaded.history.len(), 3);
    assert_committed_tool_invocation(&loaded.history);
    assert!(matches!(
        loaded.history.last(),
        Some(HistoryItem::User { content, .. }) if content.text_content() == "continue after resume"
    ));
}

#[test]
fn resuming_session_with_active_writer_lease_is_read_only() {
    let guard = test_home_guard();
    let mut writer = TestApp::builder().build_with_test_home_guard(&guard);
    writer
        .app
        .session_append_history(HistoryItem::user(Content::text("owned history")));
    writer.app.save_session_and_flush();

    let session_id = writer.app.core.session.id.clone();
    let session_dir = smelt_core::session::dir_for_id(&session_id);
    let db_path = session_dir.join("session.db");
    let before = smelt_store::SessionDb::open_read_only(&db_path)
        .unwrap()
        .session_state()
        .unwrap()
        .expect("session state before read-only resume");
    drop(writer);

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    smelt_store::SessionDb::open(&db_path)
        .unwrap()
        .set_writer_lease(&smelt_store::WriterLease {
            owner_id: "other-host:424242".into(),
            hostname: "other-host".into(),
            pid: 424_242,
            app_version: "test".into(),
            started_at: now,
            heartbeat_at: now,
        })
        .unwrap();

    let mut reader = TestApp::builder().build_without_test_home_reset(&guard);
    reader.app.load_session_by_id(&session_id);

    assert_eq!(reader.app.core.session.id, session_id);
    assert!(reader.app.session_is_read_only());
    assert_eq!(reader.app.session_history_len(), 1);

    let result = reader
        .app
        .apply_history_append_to_history(&HistoryAppend::append(HistoryItem::user(Content::text(
            "read-only mutation",
        ))));
    assert_eq!(result, HistoryAppendResult::Unchanged);
    reader.app.save_session_and_flush();

    let after = smelt_store::SessionDb::open_read_only(&db_path)
        .unwrap()
        .session_state()
        .unwrap()
        .expect("session state after read-only resume");
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.history_len, before.history_len);
    assert_eq!(
        smelt_store::SessionDb::open_read_only(&db_path)
            .unwrap()
            .history_item_count()
            .unwrap(),
        1
    );
}
