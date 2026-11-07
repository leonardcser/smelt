//! Op planning and application: maps each `FuzzOp` to an app event plus its
//! `PostCheck`, and runs macro and side-channel ops.

use crate::*;

/// Translate a `FuzzOp` into the `SourceEvent` to feed and the post-check
/// to run after. `None` for the event means the op was handled inline (via
/// a harness side channel) and the caller should not feed anything.
pub(crate) fn plan(app: &TestApp, op: FuzzOp) -> (Option<SourceEvent>, PostCheck) {
    match op {
        FuzzOp::KeyUnicode(raw) => {
            let valid = char::from_u32(raw).is_some();
            let c = decode_codepoint(raw);
            let ev = SourceEvent::Term(key_event(KeyCode::Char(c), KeyModifiers::NONE));
            let check = if valid {
                PostCheck::PromptCharInserted {
                    ch: c,
                    lua_keymap_bound: app.prompt_plain_char_has_lua_keymap(c),
                }
            } else {
                PostCheck::None
            };
            (Some(ev), check)
        }
        FuzzOp::KeyCtrl(b) => {
            let c = (b'a' + (b % 26)) as char;
            let ev = SourceEvent::Term(key_event(KeyCode::Char(c), KeyModifiers::CONTROL));
            (Some(ev), PostCheck::None)
        }
        FuzzOp::KeyShift(b) => {
            let c = (b'a' + (b % 26)) as char;
            let ev = SourceEvent::Term(key_event(KeyCode::Char(c), KeyModifiers::SHIFT));
            (Some(ev), PostCheck::None)
        }
        FuzzOp::KeySpecial(which) => {
            let code = SPECIALS[(which as usize) % SPECIALS.len()];
            (
                Some(SourceEvent::Term(key_event(code, KeyModifiers::NONE))),
                PostCheck::None,
            )
        }
        FuzzOp::KeySpecialShift(which) => {
            let code = SPECIALS[(which as usize) % SPECIALS.len()];
            (
                Some(SourceEvent::Term(key_event(code, KeyModifiers::SHIFT))),
                PostCheck::None,
            )
        }
        FuzzOp::Paste(s) => {
            let check = PostCheck::PromptTextInserted { text: s.clone() };
            (Some(SourceEvent::Term(TermEvent::Paste(s))), check)
        }
        FuzzOp::Mouse(m) => {
            let ev = MouseEvent {
                kind: decode_mouse_kind(m.kind, m.button),
                column: u16::from(m.col),
                row: u16::from(m.row),
                modifiers: decode_mouse_mods(m.mods),
            };
            (
                Some(SourceEvent::Term(TermEvent::Mouse(ev))),
                PostCheck::None,
            )
        }
        FuzzOp::Tick(ms) => (Some(SourceEvent::Tick(u64::from(ms))), PostCheck::None),
        FuzzOp::LuaWakeup => (Some(SourceEvent::LuaWakeup), PostCheck::None),
        FuzzOp::Resize { w, h } => (
            Some(SourceEvent::Resize {
                width: clamp_dim(w),
                height: clamp_dim(h),
            }),
            PostCheck::None,
        ),
        // Side-channel: not a SourceEvent.
        FuzzOp::StartTurn(_) => (None, PostCheck::None),
        FuzzOp::EngineReady => (
            Some(SourceEvent::engine(EngineEvent::Ready)),
            PostCheck::None,
        ),
        FuzzOp::EngineText(s) => (
            Some(SourceEvent::engine(EngineEvent::Text { content: s })),
            PostCheck::TextFlushed,
        ),
        FuzzOp::EngineTextDelta(s) => (
            Some(SourceEvent::engine(EngineEvent::TextDelta { delta: s })),
            PostCheck::None,
        ),
        FuzzOp::EngineThinking(s) => (
            Some(SourceEvent::engine(EngineEvent::Reasoning {
                kind: ReasoningKind::Raw,
                title: None,
                content: s,
            })),
            PostCheck::ThinkingFlushed,
        ),
        FuzzOp::EngineThinkingDelta(s) => (
            Some(SourceEvent::engine(EngineEvent::ReasoningPartDelta {
                id: "fuzz:raw:0".into(),
                kind: ReasoningKind::Raw,
                delta: s,
                title: None,
            })),
            PostCheck::None,
        ),
        FuzzOp::EngineToolStart {
            call_id,
            tool_name,
            args,
        } => {
            let invocation_id = fuzz_invocation_id(call_id);
            let cid = call_id_string(call_id);
            let ev = SourceEvent::engine(EngineEvent::ToolStarted {
                invocation_id,
                call_id: cid,
                tool_name,
                args: args.into_map(),
                called_at_ms: u64::from(call_id),
            });
            (Some(ev), PostCheck::ToolStarted { invocation_id })
        }
        FuzzOp::EngineToolOutput { call_id, chunk } => {
            let invocation_id = fuzz_invocation_id(call_id);
            let cid = call_id_string(call_id);
            let ev = SourceEvent::engine(EngineEvent::ToolOutput {
                invocation_id,
                call_id: cid,
                line: chunk.replace(['\r', '\n'], " "),
            });
            (Some(ev), PostCheck::ToolOutput { invocation_id })
        }
        FuzzOp::EngineToolFinish {
            call_id,
            is_error,
            content,
        } => {
            let invocation_id = fuzz_invocation_id(call_id);
            let cid = call_id_string(call_id);
            let ev = SourceEvent::engine(EngineEvent::ToolFinished {
                invocation_id,
                call_id: cid,
                result: ToolOutcome::new(content, is_error, None),
                elapsed_ms: Some(0),
            });
            (Some(ev), PostCheck::ToolFinished { invocation_id })
        }
        FuzzOp::ExecOutput(s) => (Some(SourceEvent::ExecOutput(s)), PostCheck::None),
        FuzzOp::ExecDone(code) => (Some(SourceEvent::ExecDone(code)), PostCheck::ExecCleared),
        FuzzOp::EngineTurnComplete { .. } => {
            // Needs the live turn_id (not accessible here); handled
            // inline in `apply` before reaching `plan`.
            unreachable!("EngineTurnComplete handled inline in apply()")
        }
        FuzzOp::EngineTurnError(message) => {
            let ev = SourceEvent::engine(EngineEvent::TurnError {
                message,
                kind: None,
                retry_at_ms: None,
            });
            (Some(ev), PostCheck::TurnErrored)
        }
        FuzzOp::EngineSteered { text, count } => {
            let n = usize::from(count);
            let item = protocol::HistoryItem::user(protocol::Content::text(text))
                .with_sent_at_ms(1_742_567_823_000);
            let ev = SourceEvent::engine(EngineEvent::Steered {
                turn_id: app.current_turn_id().unwrap_or(0),
                delta: protocol::CanonicalHistoryDelta::new(
                    app.session_message_count(),
                    vec![item; n],
                ),
            });
            (Some(ev), PostCheck::Steered { count: n })
        }
        FuzzOp::EngineRetrying { delay_ms, attempt } => {
            let ev = SourceEvent::engine(EngineEvent::Retrying {
                delay_ms: u64::from(delay_ms),
                attempt: u32::from(attempt),
            });
            (Some(ev), PostCheck::Retrying)
        }
        FuzzOp::EngineTokenUsage {
            prompt,
            completion,
            tps,
            cost_cents,
            background,
        } => {
            let prompt = u32::from(prompt);
            let completion = u32::from(completion);
            let cost_usd = f64::from(cost_cents) / 100.0;
            let usage = TokenUsage {
                context_tokens: None,
                prompt_tokens: Some(prompt),
                completion_tokens: Some(completion),
                cache_read_tokens: None,
                cache_write_tokens: None,
                reasoning_tokens: None,
            };
            let ev = SourceEvent::engine(EngineEvent::TokenUsage {
                usage,
                tokens_per_sec: Some(f64::from(tps)),
                cost_usd: Some(cost_usd),
                background,
            });
            (
                Some(ev),
                PostCheck::TokenUsageReceived {
                    prompt,
                    cost_usd,
                    background,
                },
            )
        }
        FuzzOp::PushQueuedMessage(_) => {
            unreachable!("PushQueuedMessage handled inline in apply()")
        }
        FuzzOp::JobCompleted { .. } => {
            unreachable!("JobCompleted handled inline in apply()")
        }
        FuzzOp::EngineMessages { .. } => {
            // Needs the live turn_id; handled inline in `apply` before
            // reaching `plan`.
            unreachable!("EngineMessages handled inline in apply()")
        }
        FuzzOp::EngineRequestPermission {
            req_id,
            call_id,
            tool_name,
            summary,
            args,
        } => {
            let ev = SourceEvent::engine(EngineEvent::RequestPermission {
                invocation_id: protocol::InvocationId::new(u64::from(req_id)),
                request_id: u64::from(req_id),
                call_id: call_id_string(call_id),
                tool_name,
                provider: None,
                args: args.into_map(),
                approval_patterns: Vec::new(),
                summary: protocol::style::StyledLines::from_plain(summary),
                called_at_ms: u64::from(req_id),
            });
            (Some(ev), PostCheck::PermissionRequested)
        }
        FuzzOp::EngineRequestPermissionBash {
            req_id,
            call_id,
            command,
        } => {
            let mut args = std::collections::HashMap::new();
            args.insert("command".to_string(), serde_json::Value::String(command));
            let ev = SourceEvent::engine(EngineEvent::RequestPermission {
                invocation_id: protocol::InvocationId::new(u64::from(req_id)),
                request_id: u64::from(req_id),
                call_id: call_id_string(call_id),
                tool_name: "bash".to_string(),
                provider: None,
                args,
                approval_patterns: Vec::new(),
                summary: protocol::style::StyledLines::from_plain("bash".to_string()),
                called_at_ms: u64::from(req_id),
            });
            (Some(ev), PostCheck::PermissionRequested)
        }
        FuzzOp::ApproveFirstConfirm | FuzzOp::DenyFirstConfirm { .. } => {
            unreachable!("Approve/Deny side channels handled inline in apply()")
        }
        FuzzOp::EngineToolDispatch {
            req_id,
            call_id,
            tool_name,
            args,
        } => {
            let ev = SourceEvent::engine(EngineEvent::ToolDispatch {
                invocation_id: protocol::InvocationId::new(u64::from(req_id)),
                request_id: u64::from(req_id),
                call_id: call_id_string(call_id),
                tool_name,
                args: args.into_map(),
            });
            (Some(ev), PostCheck::ToolDispatched)
        }
        FuzzOp::EngineToolEvaluationRequest {
            req_id,
            call_id,
            tool_name,
            args,
        } => {
            let ev = SourceEvent::engine(EngineEvent::ToolEvaluationRequest {
                invocation_id: protocol::InvocationId::new(u64::from(req_id)),
                request_id: u64::from(req_id),
                call_id: call_id_string(call_id),
                tool_name,
                args: args.into_map(),
                mode: AgentMode::normal(),
            });
            (Some(ev), PostCheck::ToolEvaluationRequested)
        }
        FuzzOp::EngineCoreToolResult {
            req_id,
            content,
            is_error,
        } => {
            let ev = SourceEvent::engine(EngineEvent::CoreToolResult {
                request_id: u64::from(req_id),
                content,
                is_error,
                metadata: None,
            });
            (Some(ev), PostCheck::CoreToolResultReceived)
        }
        FuzzOp::EngineShutdown { reason } => {
            let ev = SourceEvent::engine(EngineEvent::Shutdown { reason });
            (Some(ev), PostCheck::ShutdownReceived)
        }
        FuzzOp::Cancel
        | FuzzOp::Steer { .. }
        | FuzzOp::Unsteer { .. }
        | FuzzOp::CallCoreTool { .. }
        | FuzzOp::SetAgentMode { .. }
        | FuzzOp::InstallPromptCursorTrap { .. }
        | FuzzOp::StartCustomCommand { .. }
        | FuzzOp::StartEngineAsk { .. }
        | FuzzOp::ScheduleReload
        | FuzzOp::ProbePromptCursorAfterTurn { .. }
        | FuzzOp::ProbeCompactionPrepareRequest { .. }
        | FuzzOp::InsertAttachment { .. }
        | FuzzOp::TogglePaneFocus
        | FuzzOp::ReloadLua
        | FuzzOp::OpenOverlay { .. }
        | FuzzOp::InstallPlaceholder { .. }
        | FuzzOp::ClearPlaceholder
        | FuzzOp::EngineAskResponsePending { .. }
        | FuzzOp::EngineAskErrorPending { .. }
        | FuzzOp::StartExec { .. } => {
            unreachable!("side channels handled inline in apply()")
        }
        FuzzOp::EngineToolDraftDelta {
            call_id,
            tool_name,
            delta,
        } => {
            let ev = SourceEvent::engine(EngineEvent::ToolCallDraftDelta {
                stream_id: call_id.clone(),
                call_id: Some(call_id),
                tool_name: Some(tool_name),
                delta,
            });
            (Some(ev), PostCheck::None)
        }
        FuzzOp::EngineAskResponse { id, content } => {
            let ev = SourceEvent::engine(EngineEvent::EngineAskResponse {
                id,
                message: Some(Message::assistant(Some(Content::text(content)), None, None)),
                error: None,
            });
            (Some(ev), PostCheck::None)
        }
        FuzzOp::EngineAskError {
            id,
            kind_idx,
            message,
        } => {
            let kind = ENGINE_ASK_ERROR_KINDS[(kind_idx as usize) % ENGINE_ASK_ERROR_KINDS.len()];
            let ev = SourceEvent::engine(EngineEvent::EngineAskResponse {
                id,
                message: None,
                error: Some(EngineAskError { kind, message }),
            });
            (Some(ev), PostCheck::None)
        }
        FuzzOp::MacroPromptEdit { .. }
        | FuzzOp::MacroToolRoundTrip { .. }
        | FuzzOp::MacroStreamTurn { .. } => {
            unreachable!("macro ops handled inline in apply()")
        }
    }
}

pub(crate) fn apply_macro_prompt_edit(
    app: &mut TestApp,
    text: String,
    repetitions: u8,
    movement: u8,
    submit: bool,
) {
    let bounded = smelt_buffer::text::slice(&text, 0..128).to_string();
    let reps = usize::from(repetitions % 8) + 1;
    for _ in 0..reps {
        apply(app, FuzzOp::Paste(bounded.clone()));
        if app.quit_requested() {
            return;
        }
    }
    let moves = usize::from(movement % 16);
    let key = if movement & 0x80 == 0 { 7 } else { 6 }; // right / left in SPECIALS
    for _ in 0..moves {
        apply(app, FuzzOp::KeySpecial(key));
        if app.quit_requested() {
            return;
        }
    }
    if submit {
        apply(app, FuzzOp::KeySpecial(0));
    }
}

pub(crate) fn apply_macro_tool_round_trip(
    app: &mut TestApp,
    call_id: u8,
    tool_name: String,
    args: ArgsBag,
    output: String,
    is_error: bool,
) {
    if !app.agent_running() {
        app.start_turn(app.next_turn_id());
    }
    apply(
        app,
        FuzzOp::EngineToolStart {
            call_id,
            tool_name,
            args,
        },
    );
    if app.quit_requested() {
        return;
    }
    let output = smelt_buffer::text::slice(&output, 0..256).to_string();
    apply(
        app,
        FuzzOp::EngineToolOutput {
            call_id,
            chunk: output.clone(),
        },
    );
    if app.quit_requested() {
        return;
    }
    apply(
        app,
        FuzzOp::EngineToolFinish {
            call_id,
            is_error,
            content: output,
        },
    );
}

pub(crate) fn apply_macro_stream_turn(
    app: &mut TestApp,
    text: String,
    thinking: String,
    chunks: u8,
    error: bool,
) {
    if !app.agent_running() {
        app.start_turn(app.next_turn_id());
    }
    let text = smelt_buffer::text::slice(&text, 0..256).to_string();
    let thinking = smelt_buffer::text::slice(&thinking, 0..256).to_string();
    let chunks = usize::from(chunks % 8) + 1;
    for i in 0..chunks {
        if !text.is_empty() {
            apply(app, FuzzOp::EngineTextDelta(format!("{i}:{text}")));
        }
        if !thinking.is_empty() {
            apply(app, FuzzOp::EngineThinkingDelta(format!("{i}:{thinking}")));
        }
        if app.quit_requested() {
            return;
        }
    }
    if error {
        apply(app, FuzzOp::EngineTurnError("macro stream error".into()));
    } else {
        apply(app, FuzzOp::EngineTurnComplete { msg_count: 1 });
    }
}

/// Side channels are `FuzzOp` variants that bypass `feed_one_within_budget`
/// and poke the app directly (host-level pokes the engine never sees). The
/// dispatcher returns `Ok(())` if the op was a side channel and was handled,
/// or `Err(op)` if the op should fall through to the event-feeding path.
/// Owned-data variants take ownership cleanly via this single match.
pub(crate) fn try_dispatch_side_channel(app: &mut TestApp, op: FuzzOp) -> Result<(), FuzzOp> {
    match op {
        FuzzOp::PushQueuedMessage(text) => app.push_queued_message(text),
        FuzzOp::JobCompleted {
            id,
            exit_code,
            termination,
        } => {
            let pre = Snapshot::capture(app);
            app.handle_job_completed(id, exit_code, termination.into());
            // Idle completion submits a canonical turn asynchronously. Observe
            // its durable dispatch before checking the completed lifecycle.
            app.wait_for_turn_dispatch();
            app.feed_one_within_budget(SourceEvent::Tick(0), AllocBudget::DEFAULT);
            let post = Snapshot::capture(app);
            let new_actions = app.actions_since(pre.action_count);
            run_check(PostCheck::JobCompleted, &pre, &post, new_actions);
            check_turn_end_invariants(&pre, &post);
        }
        FuzzOp::InsertAttachment { label } => app.insert_attachment(label),
        FuzzOp::InstallPlaceholder { text, variant } => {
            let (accept, dismiss) = placeholder_chord_pair(variant);
            app.install_prompt_placeholder(text, accept, dismiss);
        }
        FuzzOp::ClearPlaceholder => app.clear_prompt_placeholder(),
        FuzzOp::StartTurn(_) => app.start_turn(app.next_turn_id()),
        FuzzOp::TogglePaneFocus => app.toggle_pane_focus(),
        FuzzOp::OpenOverlay { variant } => app.open_synthetic_overlay(variant),
        FuzzOp::ReloadLua => {
            app.reload_lua();
        }
        FuzzOp::ApproveFirstConfirm => {
            let pre = Snapshot::capture(app);
            app.resolve_first_confirm(true, None);
            let post = Snapshot::capture(app);
            let new_actions = app.actions_since(pre.action_count);
            run_check(
                PostCheck::ConfirmResolved {
                    approved: true,
                    had_message: false,
                },
                &pre,
                &post,
                new_actions,
            );
            check_turn_end_invariants(&pre, &post);
        }
        FuzzOp::DenyFirstConfirm { message } => {
            let pre = Snapshot::capture(app);
            let had_message = message.is_some();
            app.resolve_first_confirm(false, message);
            let post = Snapshot::capture(app);
            let new_actions = app.actions_since(pre.action_count);
            run_check(
                PostCheck::ConfirmResolved {
                    approved: false,
                    had_message,
                },
                &pre,
                &post,
                new_actions,
            );
            check_turn_end_invariants(&pre, &post);
        }
        FuzzOp::EngineAskResponsePending { content } => {
            if let Some(id) = app.pending_ask_id() {
                let ev = SourceEvent::engine(EngineEvent::EngineAskResponse {
                    id,
                    message: Some(Message::assistant(Some(Content::text(content)), None, None)),
                    error: None,
                });
                app.feed_one(ev);
            }
        }
        FuzzOp::EngineAskErrorPending { kind_idx, message } => {
            if let Some(id) = app.pending_ask_id() {
                let kind =
                    ENGINE_ASK_ERROR_KINDS[(kind_idx as usize) % ENGINE_ASK_ERROR_KINDS.len()];
                let ev = SourceEvent::engine(EngineEvent::EngineAskResponse {
                    id,
                    message: None,
                    error: Some(EngineAskError { kind, message }),
                });
                app.feed_one(ev);
            }
        }
        FuzzOp::StartExec { command } => {
            app.start_exec(&command);
        }
        FuzzOp::Cancel => {
            let pre = Snapshot::capture(app);
            app.cancel();
            let post = Snapshot::capture(app);
            let new_actions = app.actions_since(pre.action_count);
            run_check(PostCheck::TurnCancelled, &pre, &post, new_actions);
            check_turn_end_invariants(&pre, &post);
        }
        FuzzOp::Steer { text } => {
            app.steer(&text);
        }
        FuzzOp::Unsteer { count } => {
            app.unsteer(usize::from(count));
        }
        FuzzOp::CallCoreTool { tool_name, args } => {
            app.call_core_tool(&tool_name, args.into_map());
        }
        FuzzOp::SetAgentMode { mode } => {
            app.set_agent_mode(mode.into());
        }
        FuzzOp::InstallPromptCursorTrap { variant } => {
            app.install_prompt_cursor_trap(variant);
        }
        FuzzOp::StartCustomCommand { variant } => {
            let payload = app
                .start_custom_command_with_lua_tool(variant)
                .expect("custom-command side channel did not send StartTurn");
            let expected = format!("fuzz_custom_tool_{}", variant % 4);
            assert!(
                payload.tools.iter().any(|t| t.name == expected),
                "custom-command StartTurn missing registered tool {expected}: {:?}",
                payload.tools.iter().map(|t| &t.name).collect::<Vec<_>>()
            );
        }
        FuzzOp::StartEngineAsk { question } => {
            app.start_engine_ask_probe(&question);
        }
        FuzzOp::ScheduleReload => {
            let was_running = app.agent_running();
            let already_pending = app.pending_lua_reload();
            app.schedule_lua_reload();
            app.drain_idle_work();
            if was_running {
                assert!(
                    app.pending_lua_reload(),
                    "scheduled reload ran while an agent turn was active"
                );
            } else if !already_pending {
                // Idle scheduled reloads drain at the next safe point, which
                // this side channel offers immediately after scheduling.
                app.assert_lua_handles_alive();
            }
        }
        FuzzOp::MacroPromptEdit {
            text,
            repetitions,
            movement,
            submit,
        } => apply_macro_prompt_edit(app, text, repetitions, movement, submit),
        FuzzOp::MacroToolRoundTrip {
            call_id,
            tool_name,
            args,
            output,
            is_error,
        } => apply_macro_tool_round_trip(app, call_id, tool_name, args, output, is_error),
        FuzzOp::MacroStreamTurn {
            text,
            thinking,
            chunks,
            error,
            ..
        } => apply_macro_stream_turn(app, text, thinking, chunks, error),
        FuzzOp::ProbePromptCursorAfterTurn { variant } => {
            app.probe_prompt_cursor_after_turn(variant);
        }
        FuzzOp::ProbeCompactionPrepareRequest { variant } => {
            app.probe_compaction_prepare_request(variant);
        }
        other => return Err(other),
    }
    Ok(())
}

/// Apply one `FuzzOp` to a `TestApp`. Every op rolls through the same path:
/// pre-snapshot → feed event (or side-channel) → post-snapshot → check →
/// global invariants.
pub fn apply(app: &mut TestApp, op: FuzzOp) {
    let op = match try_dispatch_side_channel(app, op) {
        Ok(()) => {
            app.assert_invariants();
            return;
        }
        Err(op) => op,
    };

    let pre = Snapshot::capture(app);
    // `TurnComplete` is gated on `turn_id` matching the live agent - read
    // it here so the dispatched event hits the active arm whenever a turn
    // is running. Idle dispatch still applies for the `agent_running ==
    // false` case.
    let (ev, check) = match op {
        FuzzOp::EngineTurnComplete { msg_count } => {
            let count = usize::from(msg_count);
            let id = app.current_turn_id().unwrap_or(0);
            let first_index = pre.checkpoint_first_live_index.unwrap_or(0);
            let ev = SourceEvent::engine(EngineEvent::TurnComplete {
                turn_id: id,
                history: Some(CanonicalHistoryDelta::new(
                    first_index,
                    synth_history(count),
                )),
                meta: None,
            });
            (
                Some(ev),
                PostCheck::TurnCompleted {
                    msg_count: count,
                    targeted_active: pre.agent_running,
                },
            )
        }
        FuzzOp::EngineMessages { msg_count } => {
            let count = usize::from(msg_count);
            let id = app.current_turn_id().unwrap_or(0);
            let first_index = pre.checkpoint_first_live_index.unwrap_or(0);
            let ev = SourceEvent::engine(EngineEvent::HistoryUpdated {
                turn_id: id,
                update: CanonicalHistoryDelta::new(first_index, synth_history(count)),
            });
            (
                Some(ev),
                PostCheck::MessagesReplaced {
                    msg_count: count,
                    targeted_active: pre.agent_running,
                },
            )
        }
        op => plan(app, op),
    };
    let settles_turn_lifecycle = matches!(check, PostCheck::TurnCompleted { .. })
        || matches!(
            &ev,
            Some(SourceEvent::Term(TermEvent::Key(key))) if key.code == KeyCode::Enter
        );
    if let Some(ev) = ev {
        app.feed_one_within_budget(ev, AllocBudget::DEFAULT);
    }
    if settles_turn_lifecycle {
        // Submitted turns and queued follow-ups dispatch only after persistence
        // is durable. Keep that work outside the event budget and settle it
        // before a later macro can install a synthetic turn.
        app.wait_for_turn_dispatch();
        app.feed_one_within_budget(SourceEvent::Tick(0), AllocBudget::DEFAULT);
    }
    if matches!(check, PostCheck::ExecCleared) {
        // Finishing an exec is queued transcript work that the next frame applies.
        app.render_silent();
    }
    let post = Snapshot::capture(app);
    let new_actions = app.actions_since(pre.action_count);
    run_check(check, &pre, &post, new_actions);
    check_turn_end_invariants(&pre, &post);
    app.assert_invariants();
}
