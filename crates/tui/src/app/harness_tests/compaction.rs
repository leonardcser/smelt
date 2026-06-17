use super::*;

#[test]
fn compact_command_streams_preview_into_rendered_transcript() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(80, 24);
    app.app
        .core
        .session
        .history
        .push(protocol::HistoryItem::user(protocol::Content::text("u1")));
    app.push_assistant_text("a1");
    app.app.core.session.context_tokens = Some(500);
    app.app.core.session.context_tokens_history_len = Some(app.app.core.session.history.len());

    assert!(app.run_lua(r#"smelt.cmd.run("compact")"#));
    let ask_id = app
        .pending_ask_id()
        .expect("/compact registered ask callback");

    app.app
        .dispatch_engine_event(protocol::EngineEvent::EngineAskDelta {
            id: ask_id,
            delta: "# Goal\nstreamed via slash command".into(),
        });

    let frame = app.render_to_frame().text();
    assert!(frame.contains("compacting"), "frame: {frame}");
    assert!(
        frame.contains("streamed via slash command"),
        "frame: {frame}"
    );
}

#[test]
fn engine_ask_delta_callbacks_can_update_compaction_preview_from_dispatch() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
        smelt.engine.ask_inherited({
            messages = { { role = "user", content = "summarize" } },
            on_delta = function(delta)
                smelt.transcript._set_compaction_preview(delta)
            end,
            on_response = function() end,
        })
        "#
    ));
    let ask_id = app.pending_ask_id().expect("pending ask id");

    app.app
        .dispatch_engine_event(protocol::EngineEvent::EngineAskDelta {
            id: ask_id,
            delta: "# Goal\nstream the summary".into(),
        });

    let preview_id = app
        .app
        .transcript
        .compaction_preview_id()
        .expect("compaction preview id");
    assert!(matches!(
        app.app.transcript.history().block(preview_id),
        Some(smelt_core::transcript_model::Block::CompactionPreview { summary })
            if summary == "# Goal\nstream the summary"
    ));
}

#[test]
fn cancelled_turn_without_usage_preserves_context_token_baseline() {
    let mut app = TestApp::builder().build();
    app.app
        .core
        .session
        .history
        .push(protocol::HistoryItem::user(protocol::Content::text("u1")));
    app.push_assistant_text("a1");
    app.app.core.session.context_tokens = Some(500);
    app.app.core.session.context_tokens_history_len = Some(app.app.core.session.history.len());
    app.start_turn(7);

    app.app.discard_turn(crate::app::TurnEnd::Cancelled);

    assert_eq!(app.app.core.session.context_tokens, Some(500));
    assert_eq!(app.app.core.session.context_tokens_history_len, Some(2));
}
