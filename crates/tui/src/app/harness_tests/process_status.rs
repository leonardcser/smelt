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
fn custom_mode_get_returns_a_copy_with_default_style() {
    let mut app = TestApp::builder().build();
    let group: String = app
        .eval_lua(
            r#"
            smelt.mode.register({ name = "review" })
            local copy = smelt.mode.get("review")
            copy.hl_group = "Changed"
            return smelt.mode.get("review").hl_group
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
fn statusline_shows_running_subagents_next_to_background_processes() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(120, 12);
    assert!(app.run_lua(
        r#"
        local get = smelt.signal.get
        _G.agent_count = 0
        smelt.signal.get = function(name)
            if name == "running_subagents" then return _G.agent_count end
            if name == "running_procs" then return 2 end
            return get(name)
        end
    "#
    ));
    for count in [0, 1, 10, 0] {
        assert!(app.run_lua(&format!(
            "_G.agent_count = {count}; require('smelt.statusline').invalidate()"
        )));
        let frame = app.render_to_frame();
        let status = frame.rows.last().unwrap();
        if count == 0 {
            assert!(status.contains("2 procs"), "{status}");
            assert!(!status.contains("agent"), "{status}");
        } else {
            let label = if count == 1 {
                "1 agent".into()
            } else {
                format!("{count} agents")
            };
            assert!(status.contains(&format!("2 procs · {label}")), "{status}");
            let col = smelt_buffer::text::byte_to_cell(status, status.find(&label).unwrap());
            let color = app
                .ui_probe()
                .theme()
                .resolve(smelt_core::theme::intern("SmeltProcess"))
                .fg;
            assert_eq!(frame.styles[11][col].fg, color);
        }
    }
}

#[test]
fn subagent_completion_uses_background_notification_scheduling() {
    for busy in [false, true] {
        let mut app = TestApp::builder().build();
        if busy {
            app.start_turn(7);
        }
        app.app.handle_background_completion(protocol::HistoryNote::process_status(
            "Subagents finished: #1 completed. Use wait_agents with these IDs to read their results.",
        ));
        if busy {
            assert_eq!(app.conversation_probe().pending_history_append_count(), 1);
            assert!(app.finish_turn());
        }
        app.wait_for_session_lifecycle();
        assert!(app.agent_running());
        let commands = app.drain_engine_sends();
        let mut commands = commands
            .iter()
            .chain(app.actions().iter().filter_map(|action| match action {
                Action::EngineSend(command) => Some(command.as_ref()),
                _ => None,
            }));
        assert!(commands.any(|command| matches!(
            command,
            protocol::UiCommand::StartTurn(payload)
                if payload.input.note_ref().is_some_and(|note| note.text().starts_with("Subagents finished:"))
        )), "busy={busy}");
    }
}

#[test]
fn subagent_completion_queues_while_prompt_work_is_busy() {
    let mut app = TestApp::builder().build();
    app.type_text("unfinished prompt");
    assert!(app.run_lua("_G.busy = smelt.work.busy('syncing')"));
    app.app
        .handle_background_completion(protocol::HistoryNote::process_status(
            "Subagents finished: #1 completed.",
        ));
    assert!(!app.agent_running());
    assert!(!app.app.prompt.queue_is_empty());
    assert!(app.run_lua("assert(smelt.prompt.text() == 'unfinished prompt')"));
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

#[tokio::test(flavor = "current_thread")]
async fn completed_background_process_output_expands_in_transcript() {
    let guard = test_home_guard();
    let mut app = TestApp::builder()
        .with_vim(true)
        .build_with_test_home_guard(&guard);
    app.set_terminal_size(90, 24);
    let (completion_tx, mut completion_rx) = tokio::sync::mpsc::unbounded_channel();
    app.app.core.jobs.set_completion_sender(completion_tx);
    app.app
        .core
        .jobs
        .spawn_background(
            "printf '\\033[32mbackground stdout\\033[0m\\n'; printf 'background stderr\\n' >&2; exit 7",
            &smelt_core::process::ShellSpec::default(),
            &app.app.core.env.cwd(),
            std::time::Instant::now(),
        )
        .await
        .expect("spawn background process");
    let completion = tokio::time::timeout(Duration::from_secs(5), completion_rx.recv())
        .await
        .expect("background process should complete")
        .expect("completion notification");
    app.app.core.jobs.clear();
    app.app
        .handle_platform_event(crate::app::platform_runtime::PlatformEvent::JobCompleted(
            completion,
        ));
    app.wait_for_session_lifecycle();

    let collapsed = app.render_to_frame().text();
    assert!(collapsed.contains("exited with code 7"), "{collapsed}");
    assert!(!collapsed.contains("background stdout"), "{collapsed}");
    app.focus_transcript();
    app.configure_transcript_vim(true, VimMode::Normal);
    app.type_text("gg");
    app.press(KeyCode::Enter);

    let expanded = app.render_to_frame().text();
    assert!(expanded.contains("background stdout"), "{expanded}");
    assert!(expanded.contains("background stderr"), "{expanded}");
    app.press(KeyCode::Enter);
    assert!(!app.render_to_frame().text().contains("background stdout"));

    assert!(app.finish_turn());
    app.save_session_and_flush();
    let session_id = app.session_snapshot().id;
    drop(app);

    let mut resumed = TestApp::builder()
        .with_vim(true)
        .build_without_test_home_reset(&guard);
    resumed.set_terminal_size(90, 24);
    assert!(resumed.load_session_by_id(&session_id));
    assert!(!resumed
        .render_to_frame()
        .text()
        .contains("background stdout"));
    resumed.focus_transcript();
    resumed.configure_transcript_vim(true, VimMode::Normal);
    resumed.type_text("gg");
    resumed.press(KeyCode::Enter);
    let expanded = resumed.render_to_frame().text();
    assert!(expanded.contains("background stdout"), "{expanded}");
    assert!(expanded.contains("background stderr"), "{expanded}");
}

#[test]
fn grouped_background_process_output_expands_with_keyboard() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.set_terminal_size(90, 24);
    for (id, code, output) in [("proc_1", 0, "tests passed"), ("proc_2", 7, "build failed")] {
        let event = protocol::ProcessStatusEvent::background_process_completed(
            id,
            Some(code),
            protocol::JobTermination::Exited,
        );
        app.push_process_status(event, output);
    }
    let collapsed = app.render_to_frame().text();
    assert!(
        collapsed.contains("background processes finished: 2"),
        "{collapsed}"
    );
    assert!(!collapsed.contains("tests passed"), "{collapsed}");
    app.focus_transcript();
    app.configure_transcript_vim(true, VimMode::Normal);
    app.type_text("gg");
    app.press(KeyCode::Enter);
    let expanded = app.render_to_frame().text();
    assert!(expanded.contains("tests passed"), "{expanded}");
    assert!(expanded.contains("build failed"), "{expanded}");
    app.press(KeyCode::Enter);
    assert!(!app.render_to_frame().text().contains("tests passed"));
}

#[test]
fn background_process_output_mouse_focus_and_enter_expand_beyond_preview_limit() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut app = TestApp::builder().build();
    app.set_terminal_size(90, 24);
    assert!(app.run_lua("smelt.settings.transcript = { limits = { tool_output_rows = 3 } }"));
    let event = protocol::ProcessStatusEvent::background_process_completed(
        "proc_1",
        Some(0),
        protocol::JobTermination::Exited,
    );
    let output = (0..30)
        .map(|n| format!("line {n:02}: café\n"))
        .collect::<String>();
    app.push_process_status(event, &output);
    let collapsed = app.render_to_frame().text();
    let row = collapsed
        .lines()
        .position(|line| line.contains("30 lines of output"))
        .expect("collapsed output affordance") as u16;
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
            kind,
            row,
            column: 3,
            modifiers: KeyModifiers::empty(),
        })));
    }
    app.press(KeyCode::Enter);
    app.render_silent();
    assert!(
        transcript_total_rows(&app) >= 31,
        "expanded output must not be capped"
    );

    assert!(app.run_lua("smelt.transcript.fold_kind('process_status', 'peek')"));
    let peek = app.render_to_frame().text();
    assert!(peek.contains("line 29: café"), "{peek}");
    assert!(!peek.contains("line 00: café"), "{peek}");
    assert!(transcript_total_rows(&app) < 10);
    app.set_terminal_size(28, 24);
    app.render_silent();
    app.assert_invariants();
}

#[test]
fn job_completion_after_final_request_starts_follow_up_turn() {
    let mut app = TestApp::builder().build();
    app.start_turn(7);

    app.app
        .handle_platform_event(crate::app::platform_runtime::PlatformEvent::JobCompleted(
            smelt_core::process::JobCompletion {
                id: "4242".into(),
                exit_code: Some(1),
                termination: protocol::JobTermination::Exited,
                output: "queued output".into(),
            },
        ));
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
                        note.text() == "background process 4242 exited with code 1"
                            && note.process_output() == Some("queued output"))
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

    app.app
        .handle_platform_event(crate::app::platform_runtime::PlatformEvent::JobCompleted(
            smelt_core::process::JobCompletion {
                id: "4242".into(),
                exit_code: Some(1),
                termination: protocol::JobTermination::Exited,
                output: "queued output".into(),
            },
        ));
    assert_eq!(app.conversation_probe().pending_history_append_count(), 1);

    let outcome = app.drain_ready_engine_outputs_for_frame_to(&mut std::io::sink(), |_| {});
    app.wait_for_session_lifecycle();

    assert_eq!(
        outcome,
        crate::app::render_loop::EngineOutputDrainOutcome::FrameBoundary
    );
    assert!(app.agent_running());
    assert!(app.drain_engine_sends().iter().any(|command| matches!(
        command,
        protocol::UiCommand::StartTurn(payload)
            if payload.input.note_ref().is_some_and(|note|
                note.text() == "background process 4242 exited with code 1"
                    && note.process_output() == Some("queued output"))
    )));
}

#[test]
fn job_completion_consumed_mid_turn_does_not_start_follow_up_turn() {
    let mut app = TestApp::builder().build();
    app.start_turn(7);
    let note = protocol::HistoryNote::process_status_event(
        protocol::ProcessStatusEvent::background_process_completed(
            "4242",
            Some(0),
            protocol::JobTermination::Exited,
        ),
    );

    app.app
        .handle_platform_event(crate::app::platform_runtime::PlatformEvent::JobCompleted(
            smelt_core::process::JobCompletion {
                id: "4242".into(),
                exit_code: Some(0),
                termination: protocol::JobTermination::Exited,
                output: String::new(),
            },
        ));
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
