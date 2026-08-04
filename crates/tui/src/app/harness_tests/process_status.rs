use super::*;

#[test]
fn smelt_work_busy_pushes_token_and_flips_work_cells() {
    let mut app = TestApp::builder().build();
    let lua_ok = app.run_lua(
        r#"
                _G._busy_handle = smelt.work.busy("syncing")
            "#,
    );
    assert!(lua_ok, "smelt.work.busy snippet failed");
    app.tick_signals();
    let state: String = app
        .eval_lua(r#"return smelt.signal.get("work_state")"#)
        .expect("work_state");
    assert_eq!(state, "busy");
    let label: String = app
        .eval_lua(r#"return smelt.signal.get("work_label")"#)
        .expect("work_label");
    assert_eq!(label, "syncing");
    let (count, first_label): (i64, String) = app
        .eval_lua(
            r#"
                local s = smelt.signal.get("work_busy")
                return #s, s[1].label
                "#,
        )
        .expect("work_busy");
    assert_eq!(count, 1);
    assert_eq!(first_label, "syncing");

    let ok = app.run_lua("_G._busy_handle:remove(); _G._busy_handle = nil");
    assert!(ok);
    app.tick_signals();
    let state_after: String = app
        .eval_lua(r#"return smelt.signal.get("work_state")"#)
        .expect("work_state post-release");
    assert_eq!(state_after, "idle");
}

#[test]
fn custom_mode_without_highlight_group_uses_default_style() {
    let mut app = TestApp::builder().build();
    let group: String = app
        .eval_lua(
            r#"
            smelt.mode.register({ name = "review" })
            return smelt.mode.style("review").hl_group
            "#,
        )
        .expect("custom mode highlight group");

    assert_eq!(group, "SmeltModeDefault");
}

#[test]
fn statusline_can_truncate_items_in_the_middle() {
    let mut app = TestApp::builder().build();
    let row: String = app
        .eval_lua(
            r#"
            local bar = require("smelt._bar")
            local row = bar.compose_status({
              {
                text = "smelt/.worktrees/test",
                style = { fg = "Comment" },
                priority = 7,
                truncatable = true,
                truncate = "middle",
              },
            }, { width = 14, bg_group = "SmeltStatusBg", sep_group = "SmeltSeparator" })
            return row.text
            "#,
        )
        .expect("compose statusline");

    assert_eq!(row, "smelt/…/test  ");
}

#[test]
fn statusline_spacing_respects_text_and_block_items() {
    let mut app = TestApp::builder().build();
    let row: String = app
        .eval_lua(
            r#"
            local bar = require("smelt._bar")
            local row = bar.compose_status({
              { text = "repo", style = { fg = "Comment" } },
              { text = "tok/s", style = { fg = "Comment" } },
              { text = " INSERT ", style = { hl_group = "SmeltVimInsert" } },
              { text = " ⚡yolo ", style = { hl_group = "SmeltModeDefault" } },
              { text = "procs", style = { fg = "SmeltProcess" }, separated = true },
            }, { width = 80, bg_group = "SmeltStatusBg", sep_group = "SmeltSeparator" })
            return row.text
            "#,
        )
        .expect("compose statusline");

    assert!(
        row.starts_with("repo tok/s  INSERT  ⚡yolo  procs"),
        "{row:?}"
    );
}

#[test]
fn statusline_separates_first_inline_indicator_after_pills() {
    let mut app = TestApp::builder().build();
    let row: String = app
        .eval_lua(
            r#"
            local bar = require("smelt._bar")
            local row = bar.compose_status({
              { text = " INSERT ", style = { hl_group = "SmeltVimInsert" } },
              { text = " ⚡yolo ", style = { hl_group = "SmeltModeDefault" } },
              { text = "14 procs", style = { fg = "SmeltProcess" }, separated = true },
              { text = "permission pending", style = { fg = "SmeltAccent" }, separated = true },
            }, { width = 80, bg_group = "SmeltStatusBg", sep_group = "SmeltSeparator" })
            return row.text
            "#,
        )
        .expect("compose statusline");

    assert!(
        row.starts_with(" INSERT  ⚡yolo  14 procs · permission pending"),
        "{row:?}"
    );
}

#[test]
fn spinner_redraw_restores_the_terminal_cursor_before_displaying_the_frame() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(48, 12);
    app.start_turn(1);
    assert!(app.run_lua(
        r#"
        _G.spinner_frame = "a"
        smelt.spinner.glyph = function() return _G.spinner_frame end
        "#,
    ));

    app.render_frame_to(&mut std::io::sink());
    assert!(app.run_lua(r#"_G.spinner_frame = "b""#));

    let mut output = Vec::new();
    app.render_frame_to(&mut output);

    const END_SYNCHRONIZED_UPDATE: &[u8] = b"\x1b[?2026l";
    let frame = output
        .strip_suffix(END_SYNCHRONIZED_UPDATE)
        .expect("frame should end its synchronized update");
    assert_eq!(
        frame.last(),
        Some(&b'H'),
        "frame must restore the hidden terminal cursor after painting the spinner: {output:?}"
    );
}

#[test]
fn tick_event_advances_virtual_clock() {
    let mut app = TestApp::builder().build();
    let before = app.core_probe().clock.instant_now();
    app.feed_one(SourceEvent::Tick(500));
    let after = app.core_probe().clock.instant_now();
    assert_eq!(after - before, Duration::from_millis(500));
}

#[test]
fn process_completion_after_final_request_starts_follow_up_turn() {
    let mut app = TestApp::builder().build();
    app.start_turn(7);

    app.feed_one(SourceEvent::engine(EngineEvent::ProcessCompleted {
        id: "4242".into(),
        exit_code: Some(1),
    }));
    assert_eq!(app.conversation_probe().pending_history_append_count(), 1);

    assert!(app.finish_turn());

    assert!(app.agent_running());
    assert!(app.actions().iter().any(|action| matches!(
        action,
        Action::EngineSend(command)
            if matches!(
                command.as_ref(),
                protocol::UiCommand::StartTurn(payload)
                    if payload.input.note_ref().is_some_and(|note|
                        note.text() == "background process 4242 exited with code 1")
            )
    )));
}

#[test]
fn platform_completion_before_ready_turn_complete_starts_follow_up_turn() {
    let mut app = TestApp::builder().build();
    app.start_turn(7);
    app.inject_engine(EngineEvent::TurnComplete {
        turn_id: 7,
        history: None,
        meta: None,
    })
    .expect("queue ready turn completion");

    app.app.handle_platform_event(
        crate::app::platform_runtime::PlatformEvent::ProcessCompleted(
            smelt_core::process::ProcessCompletion {
                id: "4242".into(),
                exit_code: Some(1),
            },
        ),
    );
    assert_eq!(app.conversation_probe().pending_history_append_count(), 1);

    let outcome = app.drain_ready_engine_outputs_for_frame_to(&mut std::io::sink(), |_| {});

    assert_eq!(
        outcome,
        crate::app::render_loop::EngineOutputDrainOutcome::FrameBoundary
    );
    assert!(app.agent_running());
    assert!(app.drain_engine_sends().iter().any(|command| matches!(
        command,
        protocol::UiCommand::StartTurn(payload)
            if payload.input.note_ref().is_some_and(|note|
                note.text() == "background process 4242 exited with code 1")
    )));
}

#[test]
fn process_completion_consumed_mid_turn_does_not_start_follow_up_turn() {
    let mut app = TestApp::builder().build();
    app.start_turn(7);
    let note = protocol::HistoryNote::process_status_event(
        protocol::ProcessStatusEvent::background_process_completed("4242", Some(0)),
    );

    app.feed_one(SourceEvent::engine(EngineEvent::ProcessCompleted {
        id: "4242".into(),
        exit_code: Some(0),
    }));
    app.feed_one(SourceEvent::engine(EngineEvent::HistoryAppended {
        turn_id: 7,
        delta: protocol::CanonicalHistoryDelta::new(
            app.session_snapshot().history.len(),
            vec![protocol::HistoryItem::note(note)],
        ),
    }));
    assert_eq!(app.conversation_probe().pending_history_append_count(), 0);

    assert!(app.finish_turn());

    assert!(!app.agent_running());
}
