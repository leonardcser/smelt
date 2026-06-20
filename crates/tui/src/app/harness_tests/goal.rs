use super::*;

#[test]
fn lua_goal_renders_top_banner_not_statusline() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(60, 16);

    assert!(app.run_lua(
        r#"
            local goal = require("smelt.goal")
            assert(goal.create("finish the dedicated goal banner", { auto_continue = false }))
        "#,
    ));

    let frame = app.render_to_frame();
    assert!(
        frame.rows[0].contains(" GOAL finish the dedicated goal banner"),
        "frame:\n{}",
        frame.text()
    );
    assert!(
        frame.rows[0].trim_end().ends_with("manual"),
        "manual active goals should show right-side mode:\n{}",
        frame.text()
    );
    assert!(
        !frame.rows[15].contains("finish the dedicated goal banner"),
        "statusline should not contain the goal:\n{}",
        frame.text()
    );
    assert!(
        frame.styles[0].iter().any(|style| style.bg.is_some()),
        "banner should paint a dedicated background"
    );
}

#[test]
fn lua_goal_banner_is_fully_selectable() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(48, 16);

    assert!(app.run_lua(
        r#"
            local goal = require("smelt.goal")
            assert(goal.create("copy every cell in the top bar", { auto_continue = false }))
        "#,
    ));
    app.render_silent();

    let win_id = app
        .app
        .ui
        .named_win("smelt.headerline")
        .expect("headerline window");
    let buf_id = app.app.ui.win(win_id).expect("headerline win").buf;
    let buf = app.app.ui.buf(buf_id).expect("headerline buffer");
    let spans = buf.highlights_at(0);

    assert!(!spans.is_empty(), "banner should be highlighted");
    assert!(
        spans.iter().all(|span| span.meta.selectable),
        "every banner highlight should stay selectable: {spans:?}"
    );
}

#[test]
fn lua_goal_state_writes_nested_session_updates_immediately() {
    let mut app = TestApp::builder().build();
    let session_id = app.app.core.session.id.clone();

    assert!(app.run_lua(
        r#"
            local goal = require("smelt.goal")
            assert(goal.create("persist goal state updates", { auto_continue = true }))
            assert(goal.update_status({ progress = "1/2" }))
            assert(goal.block("waiting for persisted blocker"))
        "#,
    ));

    let state_home =
        std::env::var_os("XDG_STATE_HOME").expect("XDG_STATE_HOME set by test harness");
    let state_path = std::path::PathBuf::from(state_home)
        .join("smelt")
        .join("plugins")
        .join("goal.json");
    let raw = std::fs::read_to_string(&state_path).unwrap_or_else(|err| {
        panic!("read {}: {err}", state_path.display());
    });
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let goal = &json["sessions"][session_id.as_str()];

    assert_eq!(goal["objective"], "persist goal state updates");
    assert_eq!(goal["state"], "blocked");
    assert_eq!(goal["reason"], "waiting for persisted blocker");
    assert_eq!(goal["progress"]["label"], "1/2");
    assert_eq!(goal["auto_continue"], false);
}

#[test]
fn lua_goal_state_restores_for_same_resumed_session_id() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        let session_id = app.app.core.session.id.clone();
        assert!(app.run_lua(
            r#"
                local goal = require("smelt.goal")
                assert(goal.create("restore persisted goal on resume", { auto_continue = false }))
                assert(goal.update_status({ progress = "saved" }))
            "#,
        ));
        session_id
    };

    let mut resumed = TestApp::builder().build_without_test_home_reset(&guard);
    resumed.app.core.session.id = session_id;

    assert!(resumed.run_lua(
        r#"
            local current = assert(require("smelt.goal").current())
            assert(current.objective == "restore persisted goal on resume")
            assert(current.state == "active")
            assert(current.auto_continue == false)
            assert(current.progress.label == "saved")
        "#,
    ));
}

#[test]
fn lua_goal_banner_prefers_live_progress() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(72, 16);

    assert!(app.run_lua(
        r#"
            local goal = require("smelt.goal")
            assert(goal.create("continue implementing the full goal progress plan", { auto_continue = true, summary = "Goal progress UI" }))
            assert(goal.update_status({ progress = "Step 3/7, wiring status banner" }))
        "#,
    ));

    let frame = app.render_to_frame();
    assert!(
        frame.rows[0].contains(" GOAL Goal progress UI · Step 3/7, wiring status banner"),
        "frame:\n{}",
        frame.text()
    );
    assert!(
        frame.rows[0].trim_end().ends_with("auto"),
        "frame:\n{}",
        frame.text()
    );
    assert!(
        !frame.rows[0].contains("continue implementing the full goal progress plan"),
        "banner should be glanceable and leave the full objective for /goal status:\n{}",
        frame.text()
    );
}

#[test]
fn lua_goal_banner_keeps_progress_visible_with_long_objective() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(48, 16);

    assert!(app.run_lua(
        r#"
            local goal = require("smelt.goal")
            assert(goal.create("finish a very long objective that would otherwise hide the stage label", { auto_continue = true }))
            assert(goal.update_status({ progress = "Step 1/3, diagnosing" }))
        "#,
    ));

    let frame = app.render_to_frame();
    assert!(
        frame.rows[0].contains("Step 1/3, diagnosing"),
        "progress should stay visible even when the objective is truncated:\n{}",
        frame.text()
    );
    assert!(frame.rows[0].contains('…'), "frame:\n{}", frame.text());
    assert!(
        frame.rows[0].trim_end().ends_with("auto"),
        "frame:\n{}",
        frame.text()
    );
}

#[test]
fn lua_goal_banner_stays_above_transcript_scroll_pill() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(60, 16);

    assert!(app.run_lua(
        r#"
            local goal = require("smelt.goal")
            assert(goal.create("keep the banner visible", { auto_continue = false }))
        "#,
    ));
    app.app
        .push_block(smelt_core::transcript_model::Block::User {
            text: "earlier user message".into(),
            image_labels: Vec::new(),
        });
    for i in 0..40 {
        app.app
            .push_block(smelt_core::transcript_model::Block::Text {
                content: format!("assistant row {i:02}"),
            });
    }
    app.render_silent();
    assert!(app.run_lua(r#"smelt.win.transcript():reveal(30, { cursor = true })"#));

    let frame = app.render_to_frame();
    assert!(
        frame.rows[0].contains(" GOAL keep the banner visible"),
        "frame:\n{}",
        frame.text()
    );
    assert!(
        !frame.rows[0].contains("earlier user message"),
        "scroll pill should not cover the goal banner:\n{}",
        frame.text()
    );
}

#[test]
fn lua_goal_banner_uses_status_labels_and_unicode_ellipsis() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(36, 16);

    assert!(app.run_lua(
        r#"
            local goal = require("smelt.goal")
            assert(goal.create("finish a very long objective that must be truncated", { auto_continue = true }))
        "#,
    ));
    let frame = app.render_to_frame();
    assert!(
        frame.rows[0].starts_with(" GOAL "),
        "frame:\n{}",
        frame.text()
    );
    assert!(frame.rows[0].contains('…'), "frame:\n{}", frame.text());
    assert!(
        frame.rows[0].trim_end().ends_with("auto"),
        "frame:\n{}",
        frame.text()
    );

    assert!(app.run_lua(r#"assert(require("smelt.goal").pause())"#));
    let frame = app.render_to_frame();
    assert!(
        frame.rows[0].starts_with(" PAUSED "),
        "frame:\n{}",
        frame.text()
    );
    assert!(
        !frame.rows[0].trim_end().ends_with("paused"),
        "paused label already communicates state:\n{}",
        frame.text()
    );

    assert!(app.run_lua(r#"assert(require("smelt.goal").block("waiting"))"#));
    let frame = app.render_to_frame();
    assert!(
        frame.rows[0].contains("waiting"),
        "blocked banner should show the blocker reason:\n{}",
        frame.text()
    );
    assert!(
        frame.rows[0].starts_with(" BLOCKED "),
        "frame:\n{}",
        frame.text()
    );
    assert!(
        !frame.rows[0].trim_end().ends_with("blocked"),
        "blocked label already communicates state:\n{}",
        frame.text()
    );
}
