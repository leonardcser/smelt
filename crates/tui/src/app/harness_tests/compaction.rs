use super::*;

#[test]
fn compact_command_shows_preview_before_first_delta() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(80, 24);
    app.session_append_history(protocol::HistoryItem::user(protocol::Content::text("u1")));
    app.push_assistant_text("a1");
    app.set_context_token_baseline_for_harness(Some(500));

    assert!(app.run_lua(r#"smelt.cmd.run("compact")"#));

    let preview_id = app
        .conversation_probe()
        .transcript_compaction_preview_id()
        .expect("/compact should create a preview before the provider responds");
    assert!(matches!(
        app.conversation_probe().transcript().history().block(preview_id),
        Some(smelt_core::transcript_model::Block::CompactionPreview { summary })
            if summary.is_empty()
    ));
    let frame = app.render_to_frame().text();
    assert!(frame.contains("compacting"), "frame: {frame}");
}

#[test]
fn compact_command_streams_preview_into_rendered_transcript() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(80, 24);
    app.session_append_history(protocol::HistoryItem::user(protocol::Content::text("u1")));
    app.push_assistant_text("a1");
    app.set_context_token_baseline_for_harness(Some(500));

    assert!(app.run_lua(r#"smelt.cmd.run("compact")"#));
    let ask_id = app
        .pending_ask_id()
        .expect("/compact registered ask callback");

    app.dispatch_engine_event(protocol::EngineEvent::EngineAskDelta {
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
    app.set_context_window(Some(100));
    app.session_append_history(protocol::HistoryItem::user(protocol::Content::text("u1")));
    app.push_assistant_text("a1");
    app.session_append_history(protocol::HistoryItem::user(protocol::Content::text("u2")));
    app.start_turn(42);

    let messages = protocol::history_to_messages(&app.model_history());
    let (tx, _rx) = tokio::sync::oneshot::channel();
    {
        app.dispatch_host_call(engine::HostCall::PrepareRequest {
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

    app.dispatch_engine_event(protocol::EngineEvent::EngineAskDelta {
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
        let mut sink = std::io::sink();
        app.dispatch_engine_event_in_render_loop_to(response, &mut sink, |frame| {
            transient_frame = Some(frame)
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
        .conversation_probe()
        .transcript_compaction_preview_id()
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
        let outcome = app.drain_ready_engine_outputs_for_frame_to(&mut std::io::sink(), |frame| {
            streamed_frames.push(frame.text())
        });
        if outcome == crate::app::render_loop::EngineOutputDrainOutcome::Drained {
            break;
        }
        app.render_frame_to(&mut std::io::sink());
        streamed_frames.push(app.ui_snapshot().text());
    }

    assert!(
        streamed_frames.iter().any(|frame| frame.contains(marker)),
        "ordered prepare request skipped the transient frame: {streamed_frames:#?}"
    );
    assert!(
        app.conversation_probe()
            .transcript_compaction_preview_id()
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
    let mut settings = app.core_probe().config.settings.clone();
    settings.auto_compact = true;
    settings.compact_threshold = 0.8;
    settings.compact_keep_recent_groups = 1.0;
    app.set_settings_for_harness(settings);
    app.set_context_window(Some(100));

    app.session_append_history(protocol::HistoryItem::user(protocol::Content::text("u1")));
    app.push_assistant_text("a1");
    app.session_append_history(protocol::HistoryItem::user(protocol::Content::text("u2")));

    let messages = protocol::history_to_messages(&app.model_history());
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    {
        app.dispatch_host_call(engine::HostCall::PrepareRequest {
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
        app.dispatch_engine_event(protocol::EngineEvent::EngineAskResponse {
            id: ask_id,
            message: Some(protocol::Message::assistant(
                Some(protocol::Content::text("# Goal\nsummary")),
                None,
                None,
            )),
            error: None,
        });
        app.drive_lua_tasks();
    }
    assert!(matches!(
        rx.try_recv().expect("first prepare reply"),
        engine::HostRequestDecision::Replace { .. }
    ));
    let checkpoint = app
        .conversation_probe()
        .session()
        .checkpoint
        .as_ref()
        .expect("checkpoint installed");
    assert!(checkpoint.tokens_after_estimate.is_some());
    assert_eq!(
        checkpoint.tokens_after_estimate_history_len,
        Some(app.session_message_count())
    );

    let messages = protocol::history_to_messages(&app.model_history());
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    {
        app.dispatch_host_call(engine::HostCall::PrepareRequest {
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

    app.dispatch_engine_event(protocol::EngineEvent::EngineAskDelta {
        id: ask_id,
        delta: "# Goal\nstream the summary".into(),
    });

    let preview_id = app
        .conversation_probe()
        .transcript_compaction_preview_id()
        .expect("compaction preview id");
    assert!(matches!(
        app.conversation_probe().transcript().history().block(preview_id),
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
        app.session_append_history(protocol::HistoryItem::user(protocol::Content::text("u1")));
        app.push_assistant_text("a1");
        for index in 0..40 {
            app.push_user_block(&format!("transcript row {index}: {}", "content ".repeat(8)));
            app.push_transcript_block(smelt_core::transcript_model::Block::Text {
                content: format!("assistant row {index}: {}", "response ".repeat(8)),
            });
        }
        app.set_context_token_baseline_for_harness(Some(500));
        app.follow_transcript_tail();
        app.render_to_frame();

        assert!(app.run_lua(r#"smelt.cmd.run("compact")"#));
        let ask_id = app
            .pending_ask_id()
            .expect("/compact registered ask callback");

        if scrolled_up {
            let viewport = app
                .transcript_window()
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
                !app.transcript_window().following_tail,
                "wheel input should pin the transcript before streaming"
            );
        } else {
            assert!(app.transcript_window().following_tail);
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
                !app.transcript_window().following_tail,
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
            app.conversation_probe()
                .transcript_compaction_preview_id()
                .is_none(),
            "response should replace the transient preview"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn real_engine_responses_compaction_streams_preview_before_response() {
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn read_request(stream: &mut tokio::net::TcpStream) -> serde_json::Value {
        let mut request = Vec::new();
        let mut chunk = [0_u8; 4096];
        let header_end = loop {
            let read = stream.read(&mut chunk).await.expect("read request headers");
            assert!(read > 0, "request ended before headers");
            request.extend_from_slice(&chunk[..read]);
            if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("content length"))
            })
            .expect("request content-length");
        while request.len() < header_end + content_length {
            let read = stream.read(&mut chunk).await.expect("read request body");
            assert!(read > 0, "request ended before body");
            request.extend_from_slice(&chunk[..read]);
        }
        serde_json::from_slice(&request[header_end..header_end + content_length])
            .expect("JSON request")
    }

    fn sse(events: &[&str]) -> String {
        events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect()
    }

    // Codex and OpenAI share the OpenAI Responses wire parser. Use OpenAI
    // identity here so the test does not depend on developer OAuth credentials.
    let first_events = sse(&[
        r#"{"type":"response.created","response":{"id":"resp_compaction","status":"in_progress","output":[]}}"#,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"id":"msg_compaction","type":"message","status":"in_progress","role":"assistant","content":[]}}"#,
        r##"{"type":"response.output_text.delta","item_id":"msg_compaction","output_index":0,"content_index":0,"delta":"# Goal\nlive compaction marker"}"##,
    ]);
    let remaining_events = sse(&[
        r#"{"type":"response.output_text.delta","item_id":"msg_compaction","output_index":0,"content_index":0,"delta":" completed"}"#,
        r##"{"type":"response.output_text.done","item_id":"msg_compaction","output_index":0,"content_index":0,"text":"# Goal\nlive compaction marker completed"}"##,
        r##"{"type":"response.output_item.done","output_index":0,"item":{"id":"msg_compaction","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"# Goal\nlive compaction marker completed","annotations":[]}]}}"##,
        r##"{"type":"response.completed","response":{"id":"resp_compaction","status":"completed","output":[{"id":"msg_compaction","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"# Goal\nlive compaction marker completed","annotations":[]}]}],"usage":{"input_tokens":100,"output_tokens":5,"total_tokens":105}}}"##,
    ]);
    let full_len = first_events.len() + remaining_events.len();

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock provider");
    let address = listener.local_addr().expect("mock provider address");
    let (first_chunk_tx, first_chunk_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept provider request");
        let request = read_request(&mut stream).await;
        assert_eq!(request["stream"], true, "compaction request must stream");
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {full_len}\r\nConnection: close\r\n\r\n"
        );
        stream
            .write_all(headers.as_bytes())
            .await
            .expect("write response headers");
        stream
            .write_all(first_events.as_bytes())
            .await
            .expect("write first stream chunk");
        stream.flush().await.expect("flush first stream chunk");
        let _ = first_chunk_tx.send(());
        let _ = release_rx.await;
        stream
            .write_all(remaining_events.as_bytes())
            .await
            .expect("write final stream chunk");
        stream.flush().await.expect("flush final stream chunk");
    });

    let engine_cwd = tempfile::tempdir().expect("create engine cwd");
    let engine = engine::start(
        engine::EngineConfig::new(
            engine_cwd.path().to_path_buf(),
            Arc::new(engine::clock::RealClock),
        ),
        Box::new(engine::tools::EmptyDispatcher),
    );
    let mut app = TestApp::builder()
        .with_cwd(engine_cwd.path())
        .with_engine(engine)
        .build();
    app.set_terminal_size(80, 24);
    app.use_model(smelt_core::config::ResolvedModel {
        key: "mock/compact".into(),
        provider_name: "mock".into(),
        model_name: "compact".into(),
        display_name: None,
        api_base: format!("http://{address}"),
        api_key_env: String::new(),
        provider_type: "openai".into(),
        config: protocol::ModelConfig::default(),
    });
    app.session_append_history(protocol::HistoryItem::user(protocol::Content::text("u1")));
    app.push_assistant_text("a1");
    app.set_context_token_baseline_for_harness(Some(500));
    app.follow_transcript_tail();
    app.render_to_frame();
    assert!(app.run_lua(r#"smelt.cmd.run("compact")"#));

    let waiting_frame = app.render_to_frame().text();
    assert!(
        waiting_frame.contains("compacting"),
        "frame: {waiting_frame}"
    );
    assert!(
        !waiting_frame.contains("live compaction marker"),
        "provider delta arrived before the waiting frame: {waiting_frame}"
    );

    tokio::time::timeout(std::time::Duration::from_secs(5), first_chunk_rx)
        .await
        .expect("provider did not receive compaction request")
        .expect("mock provider stopped before first chunk");

    let mut terminal_output = Vec::new();
    let mut streamed_frame = None;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while streamed_frame.is_none() {
            let output = app
                .app
                .core
                .engine
                .recv_output()
                .await
                .expect("engine stopped while compaction streamed");
            let is_delta = matches!(
                output,
                engine::EngineOutput::Event(protocol::EngineEvent::EngineAskDelta { .. })
            );
            let output_len_before = terminal_output.len();
            app.app
                .dispatch_selected_engine_output_in_render_loop_to(output, &mut terminal_output);
            if is_delta {
                assert!(
                    terminal_output.len() > output_len_before,
                    "selected compaction delta did not paint the terminal"
                );
                streamed_frame = Some(app.ui_snapshot().text());
            }
        }
    })
    .await
    .expect("engine did not emit compaction delta");

    let frame = streamed_frame.expect("streaming frame");
    assert!(frame.contains("compacting"), "frame: {frame}");
    assert!(frame.contains("live compaction marker"), "frame: {frame}");

    let next_iteration_frame = app.render_to_frame().text();
    assert!(
        next_iteration_frame.contains("live compaction marker"),
        "next event-loop frame: {next_iteration_frame}"
    );

    let _ = release_tx.send(());
    server.await.expect("mock provider server");
}

#[test]
fn cancelled_turn_without_usage_preserves_context_token_baseline() {
    let mut app = TestApp::builder().build();
    app.session_append_history(protocol::HistoryItem::user(protocol::Content::text("u1")));
    app.push_assistant_text("a1");
    app.set_context_token_baseline_for_harness(Some(500));
    app.start_turn(7);

    app.discard_turn(crate::app::TurnEnd::Cancelled);

    assert_eq!(app.session_snapshot().context_tokens, Some(500));
    assert_eq!(app.session_snapshot().context_tokens_history_len, Some(2));
}
