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
            local row = bar.layout_strip({
              {
                text = "smelt/.worktrees/test",
                style = { fg = "Comment" },
                priority = 7,
                truncatable = true,
                truncate = "middle",
              },
            }, 14, { bg_group = "SmeltStatusBg", sep_group = "SmeltSeparator" })
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
            local row = bar.layout_strip({
              { text = "repo", style = { fg = "Comment" } },
              { text = "tok/s", style = { fg = "Comment" } },
              { text = " INSERT ", style = { hl_group = "SmeltVimInsert" } },
              { text = " ⚡yolo ", style = { hl_group = "SmeltModeDefault" } },
              { text = "procs", style = { fg = "SmeltProcess" }, separated = true },
            }, 80, { bg_group = "SmeltStatusBg", sep_group = "SmeltSeparator" })
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
            local row = bar.layout_strip({
              { text = " INSERT ", style = { hl_group = "SmeltVimInsert" } },
              { text = " ⚡yolo ", style = { hl_group = "SmeltModeDefault" } },
              { text = "14 procs", style = { fg = "SmeltProcess" }, separated = true },
              { text = "permission pending", style = { fg = "SmeltAccent" }, separated = true },
            }, 80, { bg_group = "SmeltStatusBg", sep_group = "SmeltSeparator" })
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
        _G.animation_now = 0
        smelt.time.now_ms = function() return _G.animation_now end
        "#,
    ));

    app.render_normal_to(&mut std::io::sink());
    assert!(app.run_lua("_G.animation_now = 800"));

    let mut output = Vec::new();
    app.render_normal_to(&mut output);

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
fn working_indicator_follows_the_turn_lifecycle() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(64, 10);
    app.start_turn(1);

    let bar_row = |frame: String| {
        frame
            .lines()
            .find(|line| line.starts_with('─'))
            .expect("prompt bar row")
            .to_owned()
    };
    let at = |app: &mut TestApp, now_ms: u32| {
        assert!(app.run_lua(&format!(
            "smelt.time.now_ms = function() return {now_ms} end
             smelt.prompt_bar.top.invalidate()"
        )));
        bar_row(app.render_to_frame().text())
    };

    // The turn starts with the coal catching while the word types in.
    let first = at(&mut app, 0);
    assert!(first.starts_with("─ ●"), "{first}");
    assert!(first.contains("…"), "{first}");
    assert!(!first.contains("working"), "{first}");

    let typing = at(&mut app, 180);
    assert!(typing.contains("work…"), "{typing}");
    assert!(!typing.contains("working"), "{typing}");

    let typed = at(&mut app, 600);
    assert!(typed.starts_with("─ ●"), "{typed}");
    assert!(typed.contains("working…"), "{typed}");

    // The word is steady: streaming and tool activity do not change it.
    assert!(app.run_lua(
        r#"
        smelt.events.emit("stream_delta", { kind = "text", bytes = 2, text = "hi" })
        smelt.events.emit("tool_start", { tool = "bash", args = {} })
        "#,
    ));
    let busy = at(&mut app, 2500);
    assert!(busy.starts_with("─ ●"), "{busy}");
    assert!(busy.contains("working…"), "{busy}");

    // A failed request waits out a backoff: the coal stays banked under the retry label.
    assert!(app.run_lua("smelt.signal._get = smelt.signal.get"));
    let force = |app: &mut TestApp, state: &str, label: &str, remaining_ms: u32| {
        assert!(app.run_lua(&format!(
            r#"
            local values = {{
              work_state = "{state}",
              work_label = "{label}",
              work_retry_remaining_ms = {remaining_ms},
            }}
            smelt.signal.get = function(name)
              if values[name] ~= nil then return values[name] end
              return smelt.signal._get(name)
            end
            "#
        )));
    };
    force(&mut app, "retrying", "retrying", 4000);
    at(&mut app, 2600);
    let waiting = at(&mut app, 3000);
    assert!(waiting.starts_with("─ ●"), "{waiting}");
    assert!(waiting.contains("retrying…"), "{waiting}");
    force(&mut app, "retrying", "retrying", 1000);
    let nearly = at(&mut app, 3800);
    assert!(nearly.starts_with("─ ●"), "{nearly}");
    assert!(nearly.contains("retrying…"), "{nearly}");

    // A dialog pauses the turn: a still coal and the dim paused label.
    force(&mut app, "paused", "paused", 0);
    let paused = at(&mut app, 4500);
    assert!(paused.starts_with("─ ● paused "), "{paused}");
    assert!(!paused.contains("…"), "{paused}");
    assert_eq!(paused, at(&mut app, 5100));

    // Done cools the coal to gray, and it stays.
    assert!(app.run_lua("smelt.signal.get = smelt.signal._get"));
    assert!(app.finish_turn());
    let cooling = at(&mut app, 6000);
    assert!(cooling.starts_with("─ ●"), "{cooling}");
    assert!(cooling.contains("done"), "{cooling}");
    // The fade lands on the very color the finished dot keeps: no faint attribute
    // that would make it jump darker once the fade ends.
    let dot_style = |app: &mut TestApp, now_ms: u32| {
        assert!(app.run_lua(&format!(
            "smelt.time.now_ms = function() return {now_ms} end
             smelt.prompt_bar.top.invalidate()"
        )));
        let frame = app.render_to_frame();
        let text = frame.text();
        let row = text.lines().position(|line| line.starts_with('─')).unwrap();
        frame
            .styles_text()
            .lines()
            .skip(1)
            .find_map(|line| {
                let mut parts = line.split_whitespace();
                let (y, x, len): (usize, usize, usize) = (
                    parts.next()?.parse().ok()?,
                    parts.next()?.parse().ok()?,
                    parts.next()?.parse().ok()?,
                );
                (y == row && x <= 2 && 2 < x + len).then(|| parts.collect::<Vec<_>>().join(" "))
            })
            .expect("styled dot")
    };
    let landing = dot_style(&mut app, 6990);
    let settled = dot_style(&mut app, 9000);
    assert!(!settled.contains("dim"), "{settled}");
    let channels = |style: &str| -> Vec<i32> {
        style
            .split(|c: char| !c.is_ascii_digit())
            .filter_map(|n| n.parse().ok())
            .collect()
    };
    for (end, rest) in channels(&landing).iter().zip(channels(&settled)) {
        assert!(
            (end - rest).abs() <= 2,
            "fade ends at {landing}, dot settles at {settled}"
        );
    }
    let done = at(&mut app, 8000);
    assert!(done.starts_with("─ ● done"), "{done}");
    assert!(!done.contains("working"), "{done}");
}

/// The green channel of a style string such as `fg=Rgb { r: 1, g: 2, b: 3 } attrs=bold`.
fn green(style: &str) -> i32 {
    style
        .split("g: ")
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or_else(|| panic!("no green channel in {style}"))
}

#[test]
fn working_coal_heats_while_a_tool_call_is_pending_and_cools_after() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(64, 10);
    app.start_turn(1);

    // Sampling at multiples of the 1600 ms breath keeps the breathing phase fixed.
    let coal_at = |app: &mut TestApp, now_ms: u32, emit: &str| {
        assert!(app.run_lua(&format!(
            "smelt.time.now_ms = function() return {now_ms} end
             {emit}
             smelt.prompt_bar.top.invalidate()"
        )));
        let frame = app.render_to_frame();
        green(&app.working_dot_style(&frame).expect("styled coal"))
    };

    coal_at(&mut app, 0, "");
    let idle = coal_at(&mut app, 1600, "");
    let still_idle = coal_at(&mut app, 4800, "");
    assert_eq!(
        idle, still_idle,
        "an idle turn leaves the coal at its minimum"
    );

    coal_at(
        &mut app,
        4900,
        "smelt.events.emit('tool_start', { tool = 'bash', args = {} })",
    );
    let early = coal_at(&mut app, 8000, "");
    assert!(
        early > idle,
        "a tool call warms the coal: {early} vs {idle}"
    );
    let long = coal_at(&mut app, 12800, "");
    assert!(
        long > early,
        "a pending call keeps warming it: {long} vs {early}"
    );

    coal_at(
        &mut app,
        12900,
        "smelt.events.emit('tool_end', { tool = 'bash', is_error = false, elapsed_ms = 8000 })",
    );
    let cooling = coal_at(&mut app, 17600, "");
    let cooled = coal_at(&mut app, 60800, "");
    assert!(
        cooling < long,
        "heat falls once the call ends: {cooling} vs {long}"
    );
    assert!(
        cooled <= idle + 1,
        "and settles back to the minimum: {cooled} vs {idle}"
    );
}

#[test]
fn working_word_rests_in_gray_and_a_sweep_flares_it() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(64, 10);
    app.start_turn(1);

    let first_letter = |app: &mut TestApp, now_ms: u32| {
        assert!(app.run_lua(&format!(
            "smelt.time.now_ms = function() return {now_ms} end
             smelt.prompt_bar.top.invalidate()"
        )));
        let frame = app.render_to_frame();
        green(&app.bar_cell_style(&frame, 4).expect("styled letter"))
    };

    first_letter(&mut app, 0);
    let landing = first_letter(&mut app, 50);
    let resting = first_letter(&mut app, 1900);
    let swept = first_letter(&mut app, 2250);
    let next_sweep = first_letter(&mut app, 4450);
    assert!(landing > 240, "a letter lands white-hot: {landing}");
    assert!(
        (150..=156).contains(&resting),
        "and rests at the built-in gray: {resting}"
    );
    assert!(
        swept > resting + 50,
        "the sweep flares it: {swept} vs {resting}"
    );
    assert!(
        next_sweep > resting + 50,
        "and it sweeps again: {next_sweep} vs {resting}"
    );
}

#[test]
fn working_word_sweep_is_locked_to_the_wall_clock() {
    let first_letter_at = |appeared_ms: u32, now_ms: u32| {
        let mut app = TestApp::builder().build();
        app.set_terminal_size(64, 10);
        app.start_turn(1);
        let mut paint = |now_ms: u32| {
            assert!(app.run_lua(&format!(
                "smelt.time.now_ms = function() return {now_ms} end
                 smelt.prompt_bar.top.invalidate()"
            )));
            app.render_to_frame()
        };
        paint(appeared_ms);
        let frame = paint(now_ms);
        green(&app.bar_cell_style(&frame, 4).expect("styled letter"))
    };

    for now_ms in [2250, 3000, 4450] {
        assert_eq!(
            first_letter_at(0, now_ms),
            first_letter_at(600, now_ms),
            "instances that started at different moments agree at {now_ms}"
        );
    }
}

#[test]
fn gallery_glitch_plugin_glitches_words_in_and_out_and_decodes_done() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(64, 10);
    app.start_turn(1);
    assert!(app.run_lua(include_str!("../../../../../docs/shots/plugins/glitch.lua")));

    let at = |app: &mut TestApp, now_ms: u32| {
        assert!(app.run_lua(&format!(
            "smelt.time.now_ms = function() return {now_ms} end
             smelt.prompt_bar.top.invalidate()"
        )));
        app.render_to_frame()
            .text()
            .lines()
            .find(|line| line.starts_with('─'))
            .expect("prompt bar row")
            .to_owned()
    };
    let frames = |app: &mut TestApp, from: u32, to: u32| {
        (from..to)
            .step_by(70)
            .map(|t| at(app, t))
            .collect::<Vec<_>>()
    };

    // A word starts as noise and decodes from the left.
    let first = at(&mut app, 0);
    assert!(!first.contains('▌') && !first.contains('▐'), "{first}");
    assert!(!first.contains("parsing"), "{first}");

    // It holds, with the odd frame glitched.
    let hold = frames(&mut app, 700, 2100);
    assert!(hold.iter().any(|row| row.contains("parsing")), "{hold:?}");
    assert!(hold.iter().any(|row| !row.contains("parsing")), "{hold:?}");

    // It corrupts away, and the next word decodes in.
    let corrupting = at(&mut app, 2560);
    assert!(!corrupting.contains("parsing"), "{corrupting}");
    let next = frames(&mut app, 3300, 4700);
    assert!(next.iter().any(|row| row.contains("linking")), "{next:?}");

    // Done decodes "done" in from noise one last time, then holds it still,
    // with no dot.
    assert!(app.finish_turn());
    let settling = at(&mut app, 4700);
    assert!(!settling.contains("done"), "{settling}");
    for t in [6000, 8000] {
        let done = at(&mut app, t);
        assert!(done.starts_with("─ done"), "{done}");
        assert!(!done.contains('●'), "{done}");
    }
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

    app.wait_for_turn_persistence();
    assert!(app.finish_turn());
    app.wait_for_turn_persistence();
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
        .position(|line| line.trim() == "30 lines")
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
    app.wait_for_turn_persistence();
    app.feed_one(SourceEvent::Tick(0));

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
    app.wait_for_turn_persistence();

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
