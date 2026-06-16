use super::*;

#[test]
fn turn_error_preserves_request_queue() {
    let mut app = TestApp::builder().build();
    app.start_turn(1);
    app.steer("steer during error");

    app.feed_one(SourceEvent::Engine(EngineEvent::TurnError {
        message: "connection failed".to_string(),
    }));

    assert!(!app.agent_running(), "error should end the active turn");
    assert_eq!(
        app.queued_message_count(),
        1,
        "request-stage queued input should be preserved on error"
    );
    assert_eq!(
        app.app.working.last_outcome(),
        Some(smelt_core::working::TurnOutcome::Interrupted),
        "error should archive an interrupted outcome"
    );

    // Plugins observe the turn_end cell; it must signal interruption on error.
    assert!(
        app.app
            .core
            .cells
            .get::<smelt_core::cells::TurnEnd>("turn_end")
            .is_some_and(|end| end.cancelled),
        "turn_end cell should be cancelled on error"
    );
}

#[test]
fn turn_error_preserves_turn_queue() {
    let mut app = TestApp::builder().build();
    app.start_turn(1);
    app.push_queued_message("next turn after error".to_string());

    app.feed_one(SourceEvent::Engine(EngineEvent::TurnError {
        message: "quota exceeded".to_string(),
    }));

    assert!(!app.agent_running(), "error should end the active turn");
    let state = app.state();
    assert_eq!(
        state.queued_inputs,
        vec!["next turn after error".to_string()],
        "turn-stage queued input should be preserved on error"
    );
    // A queued turn-stage message must not auto-start after an error.
    let started_turn = app.actions().iter().any(|action| match action {
        Action::EngineSend(cmd) => matches!(cmd.as_ref(), protocol::UiCommand::StartTurn(_)),
        _ => false,
    });
    assert!(
        !started_turn,
        "queued turn should not auto-start after an error"
    );

    // The status bar should record an interrupted outcome, not done.
    assert_eq!(
        app.app.working.last_outcome(),
        Some(smelt_core::working::TurnOutcome::Interrupted)
    );
}

#[test]
fn turn_complete_still_chains_queued_turn() {
    let mut app = TestApp::builder().build();
    app.start_turn(1);
    app.push_queued_message("next turn after complete".to_string());

    app.feed_one(SourceEvent::Engine(EngineEvent::TurnComplete {
        turn_id: 1,
        history: Vec::new(),
        meta: None,
    }));

    assert!(
        app.agent_running(),
        "queued turn should start on clean completion"
    );
    assert!(app.queued_message_count() == 0 || app.state().queued_inputs.is_empty());
}
