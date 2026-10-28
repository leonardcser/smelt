use super::*;

fn observed(app: &mut TestApp, status: protocol::AgentStatus) -> String {
    let parent = app.session_snapshot().id;
    app.feed_one(SourceEvent::engine(EngineEvent::AgentUpdated {
        session_id: parent.clone(),
        agent: Box::new(protocol::AgentInfo {
            id: "native-child".into(),
            ephemeral: true,
            generation: 1,
            parent_agent_id: None,
            owner: protocol::AgentOwner::ClaudeCode,
            title: "Review fixture".into(),
            task: "Read-only review".into(),
            status,
            activity: "Reviewing".into(),
            result: None,
            error: None,
            capabilities: protocol::AgentCapabilities {
                stop: true,
                follow_up: false,
            },
            usage: None,
            total_tokens: None,
            cost_usd: None,
        }),
    }));
    parent
}

#[test]
fn agent_observation_viewer_shows_nested_order_owner_usage_and_control_errors() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(110, 30);
    let parent = app.session_snapshot().id;
    for (id, owner, title) in [
        ("a", Some("b"), "Grandchild"),
        ("b", Some("z"), "Child"),
        ("z", None, "Root"),
    ] {
        app.feed_one(SourceEvent::engine(EngineEvent::AgentUpdated {
            session_id: parent.clone(),
            agent: Box::new(protocol::AgentInfo {
                id: id.into(),
                ephemeral: true,
                generation: 1,
                parent_agent_id: owner.map(str::to_owned),
                owner: protocol::AgentOwner::ClaudeCode,
                title: title.into(),
                task: "Review".into(),
                status: protocol::AgentStatus::Running,
                activity: String::new(),
                result: None,
                error: (id == "z").then(|| "Stop refused".into()),
                capabilities: protocol::AgentCapabilities {
                    stop: true,
                    follow_up: false,
                },
                usage: None,
                total_tokens: Some(7312),
                cost_usd: None,
            }),
        }));
    }
    app.run_lua_result(
        r#"
        local new_list = smelt.list.new
        agent_list_refreshes = 0
        smelt.list.new = function(opts)
            local list = new_list(opts)
            local replace = list.set_items_preserve
            function list:set_items_preserve(...)
                agent_list_refreshes = agent_list_refreshes + 1
                return replace(self, ...)
            end
            return list
        end
        require('smelt.plugins.agent_view').open()
    "#,
    )
    .unwrap();
    drive_lua_tasks(&mut app);
    let frame = app.render_to_frame().text();
    let probe = app.ui_probe();
    let win = probe.win(probe.focus().unwrap()).unwrap();
    let buf = probe.buf(win.buf).unwrap();
    assert!(buf.get_line(0).unwrap().contains("  Root"), "{frame}");
    assert!(buf.get_line(1).unwrap().contains("    Child"), "{frame}");
    assert!(
        buf.get_line(2).unwrap().contains("      Grandchild"),
        "{frame}"
    );
    for width in [60, 110] {
        app.set_terminal_size(width, 30);
        drive_lua_tasks(&mut app);
        let frame = app.render_to_frame().text();
        assert!(frame.contains("Claude Code"), "{frame}");
        assert!(frame.contains("7312 tokens (agent only)"), "{frame}");
        assert!(frame.contains("Agent error: Stop refused"), "{frame}");
    }
    app.press(KeyCode::Char('j'));
    app.feed_one(SourceEvent::engine(EngineEvent::AgentOutput {
        session_id: parent,
        agent_id: "b".into(),
        generation: 1,
        event: Box::new(EngineEvent::TextDelta {
            delta: "Streaming update".into(),
        }),
    }));
    app.feed_one(SourceEvent::Tick(300));
    drive_lua_tasks(&mut app);
    let probe = app.ui_probe();
    let win = probe.win(probe.focus().unwrap()).unwrap();
    assert_eq!(win.cursor_row(), 1);
    assert_eq!(app.lua_int_global("agent_list_refreshes"), Some(1));
    assert!(app.session_history().is_empty());
    app.press(KeyCode::Esc);
    assert!(app.state().focused_overlay.is_none());
}

#[test]
fn agent_observation_viewer_does_not_enable_delegation_or_require_a_parent_turn() {
    let mut app = TestApp::builder().build();
    let parent = observed(&mut app, protocol::AgentStatus::Running);
    app.feed_one(SourceEvent::engine(EngineEvent::AgentOutput {
        session_id: parent.clone(),
        agent_id: "native-child".into(),
        generation: 1,
        event: Box::new(EngineEvent::TextDelta {
            delta: "Child report marker".into(),
        }),
    }));
    assert!(app.run_lua(
        r#"
        assert(not smelt.agent.fork)
        require('smelt.plugins.agent_view').open()
    "#
    ));
    drive_lua_tasks(&mut app);
    assert!(app.state().focused_overlay.is_some());
    assert!(
        app.session_history().is_empty(),
        "child text must not enter parent history"
    );
    assert!(app.run_lua(
        r#"
        local runs = smelt.agent.runs(smelt.session.info().id)
        assert(#runs == 1 and runs[1].owner == 'claude_code')
        assert(runs[1].capabilities.follow_up == false)
        assert(smelt.agent.stop(smelt.session.info().id, 'native-child'))
    "#
    ));
    assert!(app.drain_engine_sends().into_iter().any(|command| matches!(command,
        protocol::UiCommand::StopAgent { session_id, agent_id } if session_id == parent && agent_id == "native-child")));
    observed(&mut app, protocol::AgentStatus::Completed);
    assert!(app.run_lua("assert(not smelt.agent.stop(smelt.session.info().id, 'native-child'))"));
    app.press(KeyCode::Esc);
    assert!(app.state().focused_overlay.is_none());
}

fn native_session(app: &mut TestApp, active: bool, working: bool) -> String {
    let session_id = app.session_snapshot().id;
    app.feed_one(SourceEvent::engine(EngineEvent::NativeSessionState {
        session_id: session_id.clone(),
        active,
        working,
    }));
    session_id
}

/// Runs a native turn whose parent completes while its session stays open
/// for background agents.
fn native_background_session(app: &mut TestApp) -> String {
    app.start_turn(1);
    let session_id = native_session(app, true, true);
    assert!(app.finish_turn());
    session_id
}

fn work_label(app: &mut TestApp) -> String {
    app.tick_signals();
    app.eval_lua(r#"return smelt.signal.get("work_label")"#)
        .expect("work_label")
}

fn started_turn(app: &TestApp) -> bool {
    app.actions().iter().any(|action| {
        matches!(action, Action::EngineSend(command)
            if matches!(command.as_ref(), protocol::UiCommand::StartTurn(_)))
    })
}

#[test]
fn native_turn_keeps_working_until_its_background_agents_settle() {
    let mut app = TestApp::builder().build();
    app.start_turn(1);
    native_session(&mut app, true, true);
    assert_eq!(work_label(&mut app), "working");
    let since = app.app.busy_stack.since();
    assert!(since.is_some());

    assert!(app.finish_turn());
    assert_eq!(work_label(&mut app), "working");
    assert_eq!(
        app.app.busy_stack.since(),
        since,
        "the work timer continues past the turn"
    );

    native_session(&mut app, true, false);
    assert_eq!(work_label(&mut app), "");
    assert!(
        app.app.core.native_session.is_some(),
        "an idle native process stays bound to the session"
    );

    native_session(&mut app, false, false);
    assert!(app.app.core.native_session.is_none());
}

#[test]
fn native_background_work_queues_input_until_the_session_settles() {
    let mut app = TestApp::builder().build();
    let session_id = native_background_session(&mut app);
    app.type_text("next question");
    app.press(KeyCode::Enter);
    assert_eq!(
        app.app.prompt.queued_texts(),
        vec!["next question".to_owned()]
    );
    app.wait_for_turn_persistence();
    app.feed_one(SourceEvent::Tick(0));
    assert!(!started_turn(&app));

    app.feed_one(SourceEvent::engine(EngineEvent::NativeSessionEvent {
        session_id: session_id.clone(),
        event: Box::new(EngineEvent::TextDelta {
            delta: "BACKGROUND_WAKE_REPLY".into(),
        }),
    }));
    app.feed_one(SourceEvent::engine(EngineEvent::NativeSessionEvent {
        session_id: session_id.clone(),
        event: Box::new(EngineEvent::HistoryAppended {
            turn_id: 1,
            delta: protocol::CanonicalHistoryDelta::new(
                0,
                vec![protocol::HistoryItem::Assistant(
                    protocol::AssistantStep::terminal(
                        Some(protocol::Content::text("BACKGROUND_WAKE_REPLY")),
                        None,
                        Vec::new(),
                    ),
                )],
            ),
        }),
    }));
    assert_eq!(app.session_history().len(), 1);
    assert_eq!(
        app.render_to_frame()
            .text()
            .matches("BACKGROUND_WAKE_REPLY")
            .count(),
        1
    );

    native_session(&mut app, true, false);
    app.wait_for_turn_persistence();
    app.feed_one(SourceEvent::Tick(0));
    assert!(app.app.prompt.queued_texts().is_empty());
    assert!(started_turn(&app));
}

#[test]
fn native_session_events_for_another_session_are_ignored() {
    let mut app = TestApp::builder().build();
    native_background_session(&mut app);
    app.feed_one(SourceEvent::engine(EngineEvent::NativeSessionEvent {
        session_id: "other-session".into(),
        event: Box::new(EngineEvent::HistoryAppended {
            turn_id: 1,
            delta: protocol::CanonicalHistoryDelta::new(
                0,
                vec![protocol::HistoryItem::Assistant(
                    protocol::AssistantStep::terminal(
                        Some(protocol::Content::text("STALE_REPLY")),
                        None,
                        Vec::new(),
                    ),
                )],
            ),
        }),
    }));
    assert!(app.session_history().is_empty());
}

#[test]
fn ctrl_c_interrupts_idle_native_background_work_and_keeps_its_session() {
    let mut app = TestApp::builder().build();
    let session_id = native_background_session(&mut app);
    app.press_mod(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(app.actions().iter().any(|action| matches!(action,
        Action::EngineSend(command) if matches!(command.as_ref(), protocol::UiCommand::Cancel))));
    assert!(!sent(&mut app, |command| matches!(
        command,
        protocol::UiCommand::EndNativeSession
    )));
    assert_eq!(app.app.core.native_session, Some(session_id));

    app.type_text("fresh prompt");
    app.press(KeyCode::Enter);
    app.wait_for_turn_persistence();
    app.feed_one(SourceEvent::Tick(0));
    assert!(
        started_turn(&app),
        "interrupted native work must not block input"
    );
}

#[test]
fn rewinding_ends_the_native_session() {
    let mut app = TestApp::builder().build();
    native_background_session(&mut app);
    app.app.rewind_to_history_index(None, false);
    assert!(sent(&mut app, |command| matches!(
        command,
        protocol::UiCommand::EndNativeSession
    )));
    assert!(app.app.core.native_session.is_none());
}

#[test]
fn native_background_work_keeps_the_working_dot_breathing() {
    let mut app = TestApp::builder().build();
    app.animate_with_virtual_clock();
    native_background_session(&mut app);
    assert_eq!(work_label(&mut app), "working");
    let styles: std::collections::HashSet<_> = (0..8)
        .map(|_| {
            let frame = app.advance_animation(200);
            app.working_dot_style(&frame).expect("working dot")
        })
        .collect();
    assert!(
        styles.len() > 1,
        "the dot must animate after the turn: {styles:?}"
    );
}

fn native_shell(started_at_ms: u64, background: bool) -> EngineEvent {
    EngineEvent::NativeProcesses {
        processes: vec![protocol::NativeProcess {
            id: "claude-code:task".into(),
            command: "npm run dev".into(),
            started_at_ms,
            background,
            output: "ready\n".into(),
        }],
    }
}

fn sent(app: &mut TestApp, expected: impl Fn(&protocol::UiCommand) -> bool) -> bool {
    app.drain_engine_sends().iter().any(expected)
}

#[test]
fn native_shells_join_the_process_list_and_stop_through_their_agent() {
    let mut app = TestApp::builder().build();
    let session_id = native_background_session(&mut app);
    let now = engine::clock::unix_time_ms(app.app.core.clock.as_ref());
    app.feed_one(SourceEvent::engine(native_shell(now - 65_000, true)));
    let (id, command, elapsed, text, running): (String, String, u64, String, bool) = app
        .eval_lua(
            r#"
            local row = smelt.process.list()[1]
            local out = smelt.process.output(row.id)
            return row.id, row.command, row.elapsed_secs, out.text, out.running
            "#,
        )
        .expect("native shell row");
    assert_eq!(
        (id.as_str(), command.as_str(), text.as_str(), running),
        ("claude-code:task", "npm run dev", "ready\n", true)
    );
    assert!(elapsed >= 65);
    // Looking at its output asks for the latest.
    assert!(sent(&mut app, |command| matches!(command,
        protocol::UiCommand::ReadNativeProcess { session_id: to, id } if *to == session_id && id == "claude-code:task")));

    assert!(app.run_lua("smelt.process.kill('claude-code:task')"));
    assert!(sent(&mut app, |command| matches!(command,
        protocol::UiCommand::StopNativeProcess { session_id: to, id } if *to == session_id && id == "claude-code:task")));
    assert!(app.run_lua("assert(not smelt.process.detach_foreground())"));

    app.feed_one(SourceEvent::engine(native_shell(now, false)));
    assert!(app.run_lua("assert(smelt.process.detach_foreground())"));
    assert!(sent(&mut app, |command| matches!(command,
        protocol::UiCommand::BackgroundNativeProcesses { session_id: to } if *to == session_id)));

    app.feed_one(SourceEvent::engine(EngineEvent::NativeProcesses {
        processes: Vec::new(),
    }));
    let rows: i64 = app.eval_lua("return #smelt.process.list()").unwrap();
    assert_eq!(rows, 0);
}
