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
fn auto_compaction_requests_frame_before_coalesced_response_clears_preview() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(80, 24);
    app.app.core.config.context_window = Some(100);
    app.app
        .core
        .session
        .history
        .push(protocol::HistoryItem::user(protocol::Content::text("u1")));
    app.push_assistant_text("a1");
    app.app
        .core
        .session
        .history
        .push(protocol::HistoryItem::user(protocol::Content::text("u2")));
    app.start_turn(42);

    let messages = protocol::history_to_messages(&app.app.model_history());
    let (tx, _rx) = tokio::sync::oneshot::channel();
    {
        let _guard = crate::lua::install_app_ptr(&mut app.app);
        app.app
            .dispatch_host_call(engine::HostCall::PrepareRequest {
                messages: engine::PreparedRequestMessages::model_only(messages),
                estimated_tokens: 200,
                reply: tx,
            });
    }
    let ask_id = app
        .drain_engine_sends()
        .into_iter()
        .filter_map(|cmd| match cmd {
            protocol::UiCommand::EngineAsk { id, stream, .. } => {
                assert!(stream, "auto-compaction EngineAsk should stream");
                Some(id)
            }
            _ => None,
        })
        .next_back()
        .expect("prepare-request compaction should issue EngineAsk");

    app.app
        .dispatch_engine_event(protocol::EngineEvent::EngineAskDelta {
            id: ask_id,
            delta: "# Goal\nstreamed before response".into(),
        });
    let response = protocol::EngineEvent::EngineAskResponse {
        id: ask_id,
        message: Some(protocol::Message::assistant(
            Some(protocol::Content::text("# Goal\nfinal summary")),
            None,
            None,
        )),
        error: None,
    };
    let mut transient_frame = None;
    {
        let _guard = crate::lua::install_app_ptr(&mut app.app);
        let mut sink = std::io::sink();
        app.app
            .dispatch_engine_event_in_render_loop_to(response, &mut sink, |app| {
                transient_frame = Some(app.ui.snapshot())
            });
    }

    let streamed_frame = transient_frame
        .expect("response should render the requested transient frame")
        .text();
    assert!(
        streamed_frame.contains("compacting"),
        "frame: {streamed_frame}"
    );
    assert!(
        streamed_frame.contains("streamed before response"),
        "frame: {streamed_frame}"
    );

    assert!(app
        .app
        .session_document
        .transcript
        .compaction_preview_id()
        .is_none());
}

#[test]
fn ordered_prepare_request_paints_transient_streaming_state() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(80, 24);
    assert!(app.run_lua(
        r#"
        smelt.engine.ask_inherited({
            messages = { { role = "user", content = "summarize" } },
            on_delta = function(delta)
                smelt.transcript._set_compaction_preview(delta)
            end,
            on_response = function()
                smelt.transcript._set_compaction_preview(nil)
            end,
        })
        "#
    ));
    let ask_id = app.pending_ask_id().expect("pending ask id");
    app.render_to_frame();

    let marker = "ordered prepare streaming marker";
    app.inject_engine(protocol::EngineEvent::EngineAskDelta {
        id: ask_id,
        delta: format!("# Goal\n{marker}"),
    })
    .expect("queue compaction preview delta");
    app.inject_engine(protocol::EngineEvent::EngineAskResponse {
        id: ask_id,
        message: Some(protocol::Message::assistant(
            Some(protocol::Content::text("# Goal\nfinal summary")),
            None,
            None,
        )),
        error: None,
    })
    .expect("queue compaction preview response");
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    assert!(
        app.inject_host_call(engine::HostCall::PrepareRequest {
            messages: engine::PreparedRequestMessages::new(Vec::new(), 0),
            estimated_tokens: 0,
            reply: tx,
        })
        .is_ok(),
        "queue prepare request"
    );

    let mut streamed_frames = Vec::new();
    loop {
        let outcome = app
            .app
            .drain_ready_engine_outputs_for_frame_to(&mut std::io::sink(), |app| {
                streamed_frames.push(app.ui.snapshot().text())
            });
        if outcome == crate::app::render_loop::EngineOutputDrainOutcome::Drained {
            break;
        }
        app.app.render_frame_to(&mut std::io::sink());
        streamed_frames.push(app.app.ui.snapshot().text());
    }

    assert!(
        streamed_frames.iter().any(|frame| frame.contains(marker)),
        "ordered prepare request skipped the transient frame: {streamed_frames:#?}"
    );
    assert!(
        app.app
            .session_document
            .transcript
            .compaction_preview_id()
            .is_none(),
        "final response should clear the transient preview"
    );
    assert!(matches!(
        rx.try_recv().expect("prepare request reply"),
        engine::HostRequestDecision::Continue
    ));
}

#[test]
fn auto_compaction_does_not_recompact_checkpoint_summary_without_new_old_groups() {
    let mut app = TestApp::builder().build();
    let mut settings = app.app.core.config.settings.clone();
    settings.auto_compact = true;
    settings.compact_threshold = 0.8;
    settings.compact_keep_recent_groups = 1.0;
    app.app.set_settings_for_harness(settings);
    app.app.core.config.context_window = Some(100);

    app.app
        .core
        .session
        .history
        .push(protocol::HistoryItem::user(protocol::Content::text("u1")));
    app.push_assistant_text("a1");
    app.app
        .core
        .session
        .history
        .push(protocol::HistoryItem::user(protocol::Content::text("u2")));

    let messages = protocol::history_to_messages(&app.app.model_history());
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    {
        let _guard = crate::lua::install_app_ptr(&mut app.app);
        app.app
            .dispatch_host_call(engine::HostCall::PrepareRequest {
                messages: engine::PreparedRequestMessages::model_only(messages),
                estimated_tokens: 200,
                reply: tx,
            });
    }
    let ask_id = app
        .drain_engine_sends()
        .into_iter()
        .filter_map(|cmd| match cmd {
            protocol::UiCommand::EngineAsk { id, .. } => Some(id),
            _ => None,
        })
        .next_back()
        .expect("first prepare request should compact");
    {
        let _guard = crate::lua::install_app_ptr(&mut app.app);
        app.app
            .dispatch_engine_event(protocol::EngineEvent::EngineAskResponse {
                id: ask_id,
                message: Some(protocol::Message::assistant(
                    Some(protocol::Content::text("# Goal\nsummary")),
                    None,
                    None,
                )),
                error: None,
            });
        app.app.drive_lua_tasks();
    }
    assert!(matches!(
        rx.try_recv().expect("first prepare reply"),
        engine::HostRequestDecision::Replace { .. }
    ));
    let checkpoint = app
        .app
        .core
        .session
        .checkpoint
        .as_ref()
        .expect("checkpoint installed");
    assert!(checkpoint.tokens_after_estimate.is_some());
    assert_eq!(
        checkpoint.tokens_after_estimate_history_len,
        Some(app.app.session_history_len())
    );

    let messages = protocol::history_to_messages(&app.app.model_history());
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    {
        let _guard = crate::lua::install_app_ptr(&mut app.app);
        app.app
            .dispatch_host_call(engine::HostCall::PrepareRequest {
                messages: engine::PreparedRequestMessages::model_only(messages),
                estimated_tokens: 200,
                reply: tx,
            });
    }

    let sends = app.drain_engine_sends();
    assert!(
        sends
            .iter()
            .all(|cmd| !matches!(cmd, protocol::UiCommand::EngineAsk { .. })),
        "second prepare re-entered compaction: {sends:?}"
    );
    assert!(matches!(
        rx.try_recv().expect("second prepare reply"),
        engine::HostRequestDecision::Continue
    ));
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
        .session_document
        .transcript
        .compaction_preview_id()
        .expect("compaction preview id");
    assert!(matches!(
        app.app.session_document.transcript.history().block(preview_id),
        Some(smelt_core::transcript_model::Block::CompactionPreview { summary })
            if summary == "# Goal\nstream the summary"
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn timed_render_loop_shows_compaction_preview_only_while_following_tail() {
    use crossterm::event::{MouseEvent, MouseEventKind};

    for scrolled_up in [false, true] {
        let mut app = TestApp::builder().build();
        app.set_terminal_size(80, 24);
        app.app
            .core
            .session
            .history
            .push(protocol::HistoryItem::user(protocol::Content::text("u1")));
        app.push_assistant_text("a1");
        for index in 0..40 {
            app.push_user_block(&format!("transcript row {index}: {}", "content ".repeat(8)));
            app.app
                .push_block(smelt_core::transcript_model::Block::Text {
                    content: format!("assistant row {index}: {}", "response ".repeat(8)),
                });
        }
        app.app.core.session.context_tokens = Some(500);
        app.app.core.session.context_tokens_history_len = Some(app.app.core.session.history.len());
        app.app.transcript_win_mut().follow_tail();
        app.render_to_frame();

        assert!(app.run_lua(r#"smelt.cmd.run("compact")"#));
        let ask_id = app
            .pending_ask_id()
            .expect("/compact registered ask callback");

        if scrolled_up {
            let viewport = app
                .app
                .transcript_win()
                .viewport
                .expect("rendered transcript viewport");
            for _ in 0..6 {
                app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
                    kind: MouseEventKind::ScrollUp,
                    column: viewport.rect.left.saturating_add(2),
                    row: viewport.rect.top.saturating_add(2),
                    modifiers: KeyModifiers::NONE,
                })));
                app.render_to_frame();
            }
            assert!(
                !app.app.transcript_win().is_following_tail(),
                "wheel input should pin the transcript before streaming"
            );
        } else {
            assert!(app.app.transcript_win().is_following_tail());
        }

        let marker = "render-loop preview marker";
        let mut source = crate::event_source::ScriptedSource::new([
            SourceEvent::Tick(20),
            SourceEvent::engine(protocol::EngineEvent::EngineAskDelta {
                id: ask_id,
                delta: "# Goal\n".into(),
            }),
            SourceEvent::Tick(20),
            SourceEvent::engine(protocol::EngineEvent::EngineAskDelta {
                id: ask_id,
                delta: marker.into(),
            }),
            SourceEvent::Tick(20),
            SourceEvent::engine(protocol::EngineEvent::EngineAskResponse {
                id: ask_id,
                message: Some(protocol::Message::assistant(
                    Some(protocol::Content::text("# Goal\nfinal checkpoint")),
                    None,
                    None,
                )),
                error: None,
            }),
        ]);
        let frames = app.run_scripted_render_loop(&mut source).await;
        let preview_frames: Vec<_> = frames
            .iter()
            .map(|frame| (frame.kind, frame.snapshot.text()))
            .filter(|(_, frame)| frame.contains(marker))
            .collect();

        if scrolled_up {
            assert!(
                preview_frames.is_empty(),
                "pinned transcript should not jump to the preview: {preview_frames:?}"
            );
            assert!(
                !app.app.transcript_win().is_following_tail(),
                "streaming preview should preserve the pinned viewport"
            );
        } else {
            assert!(
                preview_frames.iter().any(|(kind, frame)| {
                    *kind == RenderLoopFrameKind::Normal && frame.contains("compacting")
                }),
                "no normal frame rendered the streaming preview body: {frames:#?}"
            );
        }
        assert!(
            app.app
                .session_document
                .transcript
                .compaction_preview_id()
                .is_none(),
            "response should replace the transient preview"
        );
    }
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
