use super::*;

#[test]
fn turn_error_preserves_request_queue() {
    let mut app = TestApp::builder().build();
    app.start_turn(1);
    app.steer("steer during error");

    app.feed_one(SourceEvent::Engine(EngineEvent::TurnError {
        message: "connection failed".to_string(),
        kind: None,
        retry_at_ms: None,
    }));

    assert!(!app.agent_running(), "error should end the active turn");
    assert_eq!(
        app.queued_message_count(),
        1,
        "request-stage queued input should be preserved on error"
    );
    assert_eq!(
        app.app.working.last_outcome(),
        Some(smelt_core::working::TurnOutcome::Errored),
        "error should archive an error outcome"
    );

    // Plugins observe the turn_end event; it must signal interruption on error.
    assert!(
        app.app
            .core
            .signals
            .get::<smelt_core::signals::TurnEnd>("turn_end")
            .is_some_and(|end| end.cancelled),
        "turn_end event should be cancelled on error"
    );
}

#[test]
fn turn_error_preserves_turn_queue() {
    let mut app = TestApp::builder().build();
    app.start_turn(1);
    app.push_queued_message("next turn after error".to_string());

    app.feed_one(SourceEvent::Engine(EngineEvent::TurnError {
        message: "quota exceeded".to_string(),
        kind: None,
        retry_at_ms: None,
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

    // The status bar should record an error outcome, not done.
    assert_eq!(
        app.app.working.last_outcome(),
        Some(smelt_core::working::TurnOutcome::Errored)
    );
}

#[test]
fn public_status_cancelled_turn_is_idle_interrupted() {
    let mut app = TestApp::builder().build();
    app.app.term_focused = false;
    app.start_turn(1);

    app.app.discard_turn(crate::app::TurnEnd::Cancelled);
    app.app.publish_public_status();

    let status = smelt_core::public_status::read_status_for_pid(std::process::id()).unwrap();
    assert_eq!(status.state, smelt_core::public_status::PublicState::Idle);
    assert_eq!(
        status.reason,
        Some(smelt_core::public_status::PublicReason::Interrupted)
    );
}

#[test]
fn public_status_turn_error_needs_attention() {
    let mut app = TestApp::builder().build();
    app.start_turn(1);

    app.feed_one(SourceEvent::Engine(EngineEvent::TurnError {
        message: "connection failed".to_string(),
        kind: None,
        retry_at_ms: None,
    }));
    app.app.publish_public_status();

    let status = smelt_core::public_status::read_status_for_pid(std::process::id()).unwrap();
    assert_eq!(
        status.state,
        smelt_core::public_status::PublicState::NeedsAttention
    );
    assert_eq!(
        status.reason,
        Some(smelt_core::public_status::PublicReason::Error)
    );
}

#[test]
fn resumable_turn_error_publishes_continuation_token() {
    let mut app = TestApp::builder().build();
    app.start_turn(1);

    app.feed_one(SourceEvent::Engine(EngineEvent::TurnError {
        message: "quota exceeded".to_string(),
        kind: Some(protocol::EngineAskErrorKind::Quota),
        retry_at_ms: Some(123_000),
    }));

    let turn_end = app
        .app
        .core
        .signals
        .get::<smelt_core::signals::TurnEnd>("turn_end")
        .expect("turn_end should be published");
    assert!(turn_end.cancelled);
    assert_eq!(turn_end.error_kind.as_deref(), Some("quota"));
    assert_eq!(turn_end.retry_at_ms, Some(123_000));
    assert!(turn_end.continuation_token.is_some());
}

#[test]
fn non_quota_retry_metadata_does_not_publish_continuation_token() {
    let mut app = TestApp::builder().build();
    app.start_turn(1);

    app.feed_one(SourceEvent::Engine(EngineEvent::TurnError {
        message: "network failed".to_string(),
        kind: Some(protocol::EngineAskErrorKind::Network),
        retry_at_ms: Some(123_000),
    }));

    let turn_end = app
        .app
        .core
        .signals
        .get::<smelt_core::signals::TurnEnd>("turn_end")
        .expect("turn_end should be published");
    assert!(turn_end.cancelled);
    assert_eq!(turn_end.error_kind.as_deref(), Some("network"));
    assert_eq!(turn_end.retry_at_ms, Some(123_000));
    assert!(turn_end.continuation_token.is_none());
}

fn run_due_timers(app: &mut TestApp, ms: u64) -> Vec<protocol::UiCommand> {
    app.feed_one(SourceEvent::Tick(ms));
    app.app.tick_timers();
    app.drain_engine_sends()
}

fn has_started_turn(cmds: &[protocol::UiCommand]) -> bool {
    cmds.iter()
        .any(|cmd| matches!(cmd, protocol::UiCommand::StartTurn(_)))
}

fn clear_goal(app: &mut TestApp) {
    assert!(app.run_lua(r#"require("smelt.goal").clear()"#));
}

fn create_auto_goal(app: &mut TestApp, objective: &str) {
    clear_goal(app);
    assert!(app.run_lua(&format!(
        r#"assert(require("smelt.goal").create({objective:?}, {{ auto_continue = true }}))"#
    )));
}

fn isolated_app() -> (std::sync::MutexGuard<'static, ()>, TestApp) {
    let home_guard = crate::app::test_harness::test_home_guard();
    let app = TestApp::builder().build_with_test_home_guard(&home_guard);
    (home_guard, app)
}

#[test]
fn goal_auto_continues_after_recoverable_quota_error() {
    let (_home_guard, mut app) = isolated_app();
    create_auto_goal(&mut app, "finish quota test");
    app.start_turn(1);

    app.feed_one(SourceEvent::Engine(EngineEvent::TurnError {
        message: "quota exceeded".to_string(),
        kind: Some(protocol::EngineAskErrorKind::Quota),
        retry_at_ms: Some(0),
    }));

    assert!(has_started_turn(&run_due_timers(&mut app, 1300)));
}

#[test]
fn auto_continue_off_disables_quota_retry() {
    let (_home_guard, mut app) = isolated_app();
    assert!(app.run_lua(r#"smelt.settings.auto_continue = "off""#));
    create_auto_goal(&mut app, "finish quota test");
    app.start_turn(1);

    app.feed_one(SourceEvent::Engine(EngineEvent::TurnError {
        message: "quota exceeded".to_string(),
        kind: Some(protocol::EngineAskErrorKind::Quota),
        retry_at_ms: Some(0),
    }));

    assert!(!has_started_turn(&run_due_timers(&mut app, 1300)));
}

#[test]
fn auto_continue_always_continues_without_goal() {
    let (_home_guard, mut app) = isolated_app();
    assert!(app.run_lua(r#"smelt.settings.auto_continue = "always""#));
    clear_goal(&mut app);
    app.start_turn(1);

    app.feed_one(SourceEvent::Engine(EngineEvent::TurnComplete {
        turn_id: 1,
        first_changed_index: 0,
        history: None,
        meta: None,
    }));

    assert!(has_started_turn(&run_due_timers(&mut app, 1300)));
}

#[test]
fn auto_continue_goal_mode_ignores_sessions_without_goal() {
    let (_home_guard, mut app) = isolated_app();
    clear_goal(&mut app);
    app.start_turn(1);

    app.feed_one(SourceEvent::Engine(EngineEvent::TurnComplete {
        turn_id: 1,
        first_changed_index: 0,
        history: None,
        meta: None,
    }));

    assert!(!has_started_turn(&run_due_timers(&mut app, 1300)));
}

#[test]
fn goal_auto_continue_ignores_non_quota_errors() {
    let (_home_guard, mut app) = isolated_app();
    create_auto_goal(&mut app, "finish quota test");
    app.start_turn(1);

    app.feed_one(SourceEvent::Engine(EngineEvent::TurnError {
        message: "network failed".to_string(),
        kind: Some(protocol::EngineAskErrorKind::Network),
        retry_at_ms: Some(0),
    }));

    assert!(!has_started_turn(&run_due_timers(&mut app, 1300)));
}

#[test]
fn turn_complete_still_chains_queued_turn() {
    let mut app = TestApp::builder().build();
    app.start_turn(1);
    app.push_queued_message("next turn after complete".to_string());

    app.feed_one(SourceEvent::Engine(EngineEvent::TurnComplete {
        turn_id: 1,
        first_changed_index: 0,
        history: None,
        meta: None,
    }));

    assert!(
        app.agent_running(),
        "queued turn should start on clean completion"
    );
    assert!(app.queued_message_count() == 0 || app.state().queued_inputs.is_empty());
}
