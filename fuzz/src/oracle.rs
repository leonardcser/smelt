//! Post-condition oracles: pre/post `Snapshot` capture and the `PostCheck`
//! assertions each planned op must satisfy.

use crate::*;

/// Cheap snapshot of state needed by event-specific post-checks. Captured
/// before dispatch so a `PostCheck` can compare pre/post without re-deriving
/// what it cares about from scratch.
pub(crate) struct Snapshot {
    pub(crate) agent_running: bool,
    pending: Vec<protocol::InvocationId>,
    streaming: tui::app::test_harness::StreamingState,
    session_messages: usize,
    queued_messages: usize,
    queue_empty: bool,
    turn_paused: bool,
    queued_next_starts: bool,
    follow_up_context_append: bool,
    follow_up_context_append_unfiltered: bool,
    working: tui::app::test_harness::WorkingSnapshot,
    session_cost_usd: f64,
    context_tokens: Option<u32>,
    pub(crate) checkpoint_first_live_index: Option<usize>,
    pending_history_appends: usize,
    pending_history_follow_up: bool,
    pending_history_follow_up_unfiltered: bool,
    submitted_history_idx: Option<usize>,
    pending_lua_reload: bool,
    transcript_blocks: usize,
    pending_confirms: usize,
    deferred_dialogs: usize,
    prompt_source: String,
    prompt_cpos: usize,
    prompt_vim_mode: VimMode,
    prompt_selection_range: Option<(usize, usize)>,
    prompt_text_input_ready: bool,
    prompt_paste_target_ready: bool,
    /// Length of the harness action log at capture time. Post-checks inspect
    /// actions produced after this snapshot.
    pub(crate) action_count: usize,
}

impl Snapshot {
    pub(crate) fn capture(app: &TestApp) -> Self {
        let prompt_selection_range = app.prompt_selection_range();
        Self {
            agent_running: app.agent_running(),
            pending: app.pending_tool_invocation_ids(),
            streaming: app.streaming_state(),
            session_messages: app.session_message_count(),
            queued_messages: app.queued_message_count(),
            queue_empty: app.queue_is_empty(),
            turn_paused: app.turn_is_paused(),
            queued_next_starts: app.next_queued_input_starts_turn(),
            follow_up_context_append: app.next_follow_up_appends_context_note(),
            follow_up_context_append_unfiltered: app
                .next_follow_up_appends_context_note_unfiltered(),
            working: app.working_state(),
            session_cost_usd: app.session_cost_usd(),
            context_tokens: app.context_tokens(),
            checkpoint_first_live_index: app.checkpoint_first_live_index(),
            pending_history_appends: app.pending_history_append_count(),
            pending_history_follow_up: app.has_pending_history_follow_up(),
            pending_history_follow_up_unfiltered: app.has_pending_history_follow_up_unfiltered(),
            submitted_history_idx: app.submitted_history_idx(),
            pending_lua_reload: app.pending_lua_reload(),
            transcript_blocks: app.transcript_block_count(),
            pending_confirms: app.pending_confirm_count(),
            deferred_dialogs: app.pending_deferred_dialog_count(),
            prompt_source: app.state().prompt_text,
            prompt_cpos: app.prompt_cpos(),
            prompt_vim_mode: app.state().vim_mode,
            prompt_selection_range,
            prompt_text_input_ready: app.prompt_text_input_ready(),
            prompt_paste_target_ready: app.prompt_paste_target_ready(prompt_selection_range),
            action_count: app.actions().len(),
        }
    }
}

pub(crate) fn expected_model_history_len(pre: &Snapshot, msg_count: usize) -> usize {
    pre.checkpoint_first_live_index.unwrap_or(0) + msg_count
}

/// Whether the turn's final history drops the item the active turn submitted.
pub(crate) fn submitted_item_dropped(pre: &Snapshot, msg_count: usize) -> bool {
    let first_index = pre.checkpoint_first_live_index.unwrap_or(0);
    pre.submitted_history_idx
        .is_some_and(|idx| idx >= first_index && idx - first_index >= msg_count)
}

/// Whether a deferred note still needs to start a follow-up once the turn's
/// final history is merged. A note identical to the submitted item is delivered
/// by that item unless the incoming history replaces the item away.
pub(crate) fn pending_follow_up_after_merge(pre: &Snapshot, msg_count: usize) -> bool {
    if submitted_item_dropped(pre, msg_count) {
        pre.pending_history_follow_up_unfiltered
    } else {
        pre.pending_history_follow_up
    }
}

pub(crate) fn completed_turn_starts_queued_followup(
    pre: &Snapshot,
    msg_count: usize,
    targeted_active: bool,
) -> bool {
    targeted_active
        && (pre.queued_next_starts || pending_follow_up_after_merge(pre, msg_count))
        && !pre.working.busy
}

pub(crate) fn expected_completed_history_len(
    pre: &Snapshot,
    msg_count: usize,
    targeted_active: bool,
) -> usize {
    let queued_followup = completed_turn_starts_queued_followup(pre, msg_count, targeted_active);
    let context_append = if submitted_item_dropped(pre, msg_count) {
        pre.follow_up_context_append_unfiltered
    } else {
        pre.follow_up_context_append
    };
    let queued_context_append = usize::from(queued_followup && context_append);
    let queued_request_append = usize::from(queued_followup);
    expected_model_history_len(pre, msg_count) + queued_context_append + queued_request_append
}

pub(crate) fn scheduled_reload_mode_change(
    pre: &Snapshot,
    targeted_active: bool,
    new_actions: &[Action],
) -> bool {
    pre.pending_lua_reload
        && targeted_active
        && count_action(new_actions, |c| matches!(c, UiCommand::SetMode { .. })) > 0
}

pub(crate) fn normalize_prompt_paste_text(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace(smelt_buffer::ATTACHMENT_MARKER, "")
}

pub(crate) fn count_action<F>(actions: &[Action], pred: F) -> usize
where
    F: Fn(&UiCommand) -> bool,
{
    actions
        .iter()
        .filter(|a| matches!(a, Action::EngineSend(cmd) if pred(cmd.as_ref())))
        .count()
}

/// Post-dispatch invariant tied to a specific `FuzzOp`. Holds just the
/// payload the check needs; the pre/post `Snapshot`s carry everything else.
/// Variants gated on `agent_running` self-skip in idle dispatch, where the
/// relevant event arms are no-ops.
pub(crate) enum PostCheck {
    None,
    /// `Text` commits any streaming text buffer. Cascade: `flush_streaming_text`
    /// also flushes thinking, so both buffers must be empty after.
    TextFlushed,
    /// `Thinking { content }` commits any streaming thinking buffer.
    ThinkingFlushed,
    /// `ToolStarted` flushes streaming text + thinking and adds the invocation
    /// identity to pending.
    ToolStarted {
        invocation_id: protocol::InvocationId,
    },
    /// `ToolOutput` for an already-pending invocation is a pure append to that
    /// tool's output; the pending entry stays put.
    ToolOutput {
        invocation_id: protocol::InvocationId,
    },
    /// `ToolFinished` clears the invocation from pending when it was present.
    ToolFinished {
        invocation_id: protocol::InvocationId,
    },
    /// `ExecDone` finishes the exec block and clears `stream_exec_id`.
    ExecCleared,
    /// `TurnComplete` against the active turn. Non-empty messages replace
    /// session.messages; an active turn ends.
    TurnCompleted {
        msg_count: usize,
        targeted_active: bool,
    },
    /// `TurnError`. When an active turn was running, it ends.
    TurnErrored,
    /// `Steered`. Active-turn dispatch flushes streams and drains up to
    /// `count` request-queued messages.
    Steered {
        count: usize,
    },
    /// `Retrying`. Active-turn dispatch moves working into `Retrying`
    /// phase (still animating).
    Retrying,
    /// `TokenUsage`. Accumulates cost monotonically and (when non-background
    /// and prompt > 0) updates `context_tokens`.
    TokenUsageReceived {
        prompt: u32,
        cost_usd: f64,
        background: bool,
    },
    /// Job completion immediately pushes a transcript block when idle.
    JobCompleted,
    /// `Messages` against the active turn (matching turn_id) merges
    /// `session.messages` mid-turn; idle dispatch is a no-op.
    MessagesReplaced {
        msg_count: usize,
        targeted_active: bool,
    },
    /// `RequestPermission` against an active turn lands on exactly one of
    /// three branches: auto-approve (one new `PermissionDecision`,
    /// no new confirm), defer (one new `pending_dialogs` entry, no new
    /// confirm or decision), or register (one new entry in `core.confirms`,
    /// no decision yet).
    PermissionRequested,
    /// Approving / denying a confirm consumes one pending entry and queues
    /// a `PermissionDecision`. Approve never ends the turn; deny without a
    /// message ends it.
    ConfirmResolved {
        approved: bool,
        had_message: bool,
    },
    /// `ToolDispatch` with an unregistered tool produces exactly one
    /// `ToolResult` UiCommand (the synthetic error path). With a real Lua
    /// tool registered it could yield `Pending` instead, but the harness
    /// loads no tools so strict equality holds.
    ToolDispatched,
    /// `ToolEvaluationRequest` always produces exactly one `ToolEvaluationResponse`
    /// UiCommand, regardless of whether hooks are registered.
    ToolEvaluationRequested,
    /// `CoreToolResult` with no pending Lua coroutine is silently dropped;
    /// only the no-panic invariant matters.
    CoreToolResultReceived,
    /// `Shutdown` ends any active turn.
    ShutdownReceived,
    /// `Cancel` side channel ends any active turn and flushes streaming.
    TurnCancelled,
    /// Plain prompt character insertion should mutate source at the old cursor
    /// and advance the cursor by the inserted UTF-8 width.
    PromptCharInserted {
        ch: char,
        lua_keymap_bound: bool,
    },
    /// Paste into a ready prompt should insert the normalized paste text at the
    /// old cursor and advance by the inserted byte length.
    PromptTextInserted {
        text: String,
    },
}

pub(crate) fn run_check(check: PostCheck, pre: &Snapshot, post: &Snapshot, new_actions: &[Action]) {
    match check {
        PostCheck::None => {}
        PostCheck::TextFlushed => {
            if pre.agent_running {
                assert!(
                    !post.streaming.text && !post.streaming.thinking,
                    "EngineEvent::Text left streaming active: {:?}",
                    post.streaming
                );
            }
        }
        PostCheck::ThinkingFlushed => {
            if pre.agent_running {
                assert!(
                    !post.streaming.thinking,
                    "EngineEvent::Reasoning left streaming thinking active: {:?}",
                    post.streaming
                );
            }
        }
        PostCheck::ToolOutput { invocation_id } => {
            if pre.pending.contains(&invocation_id) {
                let count = post
                    .pending
                    .iter()
                    .filter(|id| **id == invocation_id)
                    .count();
                assert!(
                    count == 1,
                    "ToolOutput({invocation_id:?}) disturbed pending entry: {count} entries, pre {:?} post {:?}",
                    pre.pending,
                    post.pending
                );
            }
        }
        PostCheck::ToolStarted { invocation_id } => {
            if pre.agent_running {
                if !pre.pending.contains(&invocation_id) {
                    // Fresh ToolStarted flushes streaming text + thinking
                    // before pushing the tool block.
                    assert!(
                        !post.streaming.text && !post.streaming.thinking,
                        "ToolStarted({invocation_id:?}) left streaming active: {:?}",
                        post.streaming
                    );
                }
                // Duplicate dispatch must be a no-op on pending; either way,
                // the invocation ends up in `pending` exactly once.
                let count = post
                    .pending
                    .iter()
                    .filter(|id| **id == invocation_id)
                    .count();
                assert!(
                    count == 1,
                    "ToolStarted({invocation_id:?}) yields {count} pending entries, expected 1: {:?}",
                    post.pending
                );
            }
        }
        PostCheck::ToolFinished { invocation_id } => {
            if pre.pending.contains(&invocation_id) {
                assert!(
                    !post.pending.contains(&invocation_id),
                    "ToolFinished({invocation_id:?}) left pending entry: {:?}",
                    post.pending
                );
            }
        }
        PostCheck::ExecCleared => {
            assert!(
                !post.streaming.exec,
                "ExecDone left active-exec state: {:?}",
                post.streaming
            );
        }
        PostCheck::TurnCompleted {
            msg_count,
            targeted_active,
        } => {
            // Either active-turn dispatch (matching turn_id) or idle
            // dispatch with non-empty messages merges history. A compaction
            // checkpoint preserves its pre-summary prefix before appending
            // incoming model messages.
            let history_path = targeted_active || msg_count > 0;
            if history_path {
                let expected = expected_completed_history_len(pre, msg_count, targeted_active);
                let pending_appends = if targeted_active {
                    pre.pending_history_appends
                        .saturating_sub(usize::from(pending_follow_up_after_merge(pre, msg_count)))
                } else {
                    0
                };
                if scheduled_reload_mode_change(pre, targeted_active, new_actions) {
                    // A reload deferred until turn end can invalidate the
                    // active mode and switch back to normal mode. That queues
                    // a mode-change history note after the final model history
                    // snapshot has been merged; depending on whether the
                    // merged tail is already a mode note, it either appends or
                    // replaces in place.
                    let max_expected = expected + pending_appends + usize::from(expected > 0);
                    assert!(
                        (expected..=max_expected).contains(&post.session_messages),
                        "TurnComplete did not merge session.messages with scheduled reload mode change: post {} (expected {expected}..={max_expected}, incoming {msg_count}, checkpoint prefix {:?}, pending appends {}, queued follow-up {}, queued context append {}, busy {})",
                        post.session_messages,
                        pre.checkpoint_first_live_index,
                        pending_appends,
                        pre.queued_next_starts,
                        pre.follow_up_context_append,
                        pre.working.busy,
                    );
                } else {
                    let max_expected = expected + pending_appends;
                    assert!(
                        (expected..=max_expected).contains(&post.session_messages),
                        "TurnComplete did not merge session.messages: post {} (expected {expected}..={max_expected}, incoming {msg_count}, checkpoint prefix {:?}, pending appends {}, queued follow-up {}, queued context append {}, busy {})",
                        post.session_messages,
                        pre.checkpoint_first_live_index,
                        pending_appends,
                        pre.queued_next_starts,
                        pre.follow_up_context_append,
                        pre.working.busy,
                    );
                }
            }
            if targeted_active
                && !completed_turn_starts_queued_followup(pre, msg_count, targeted_active)
            {
                assert!(
                    !post.agent_running,
                    "TurnComplete against active turn did not end it",
                );
            }
        }
        PostCheck::TurnErrored => {
            if pre.agent_running {
                assert!(!post.agent_running, "TurnError did not end the active turn",);
            }
        }
        PostCheck::TokenUsageReceived {
            prompt,
            cost_usd,
            background,
        } => {
            // Idle dispatch drops TokenUsage entirely (falls through to
            // the `_ => {}` arm); only the active-turn path mutates
            // session state.
            if pre.agent_running {
                let expected = pre.session_cost_usd + cost_usd;
                assert!(
                    (post.session_cost_usd - expected).abs() < 1e-6,
                    "TokenUsage did not add cost {cost_usd}: pre {} → post {} (expected {expected})",
                    pre.session_cost_usd,
                    post.session_cost_usd,
                );
                if !background && prompt > 0 {
                    assert_eq!(
                        post.context_tokens,
                        Some(prompt),
                        "TokenUsage(prompt={prompt}, background=false) did not set context_tokens",
                    );
                }
            } else {
                assert_eq!(
                    post.session_cost_usd, pre.session_cost_usd,
                    "TokenUsage in idle dispatch should not accumulate cost",
                );
            }
        }
        PostCheck::JobCompleted => {
            // A queued input or a paused turn holds the completion back.
            if !pre.agent_running && !pre.working.busy && pre.queue_empty && !pre.turn_paused {
                assert_eq!(
                    post.transcript_blocks,
                    pre.transcript_blocks + 1,
                    "job completion did not push exactly one transcript block: pre {} -> post {}",
                    pre.transcript_blocks,
                    post.transcript_blocks,
                );
            }
        }
        PostCheck::MessagesReplaced {
            msg_count,
            targeted_active,
        } => {
            // Only the active-turn arm with matching turn_id calls
            // set_history; idle dispatch is an explicit no-op.
            if targeted_active {
                let expected = expected_model_history_len(pre, msg_count);
                assert_eq!(
                    post.session_messages, expected,
                    "Messages did not merge session.messages: post {} (expected {expected}, incoming {msg_count}, checkpoint prefix {:?})",
                    post.session_messages,
                    pre.checkpoint_first_live_index,
                );
                // Doesn't end the turn.
                assert!(
                    post.agent_running,
                    "Messages on active turn ended it unexpectedly",
                );
            } else {
                assert_eq!(
                    post.session_messages, pre.session_messages,
                    "Messages in idle dispatch should not change session.messages",
                );
            }
        }
        PostCheck::Retrying => {
            if pre.agent_running {
                assert!(
                    post.working.animating,
                    "Retrying on active turn left working idle: {:?}",
                    post.working
                );
                assert!(
                    !post.streaming.text && !post.streaming.thinking,
                    "Retrying left streaming active: {:?}",
                    post.streaming
                );
            }
        }
        PostCheck::Steered { count } => {
            // Idle dispatch is a no-op for Steered; only enforce on active.
            if pre.agent_running {
                assert!(
                    !post.streaming.text && !post.streaming.thinking,
                    "Steered left streaming active: {:?}",
                    post.streaming
                );
                let drained = count.min(pre.queued_messages);
                assert_eq!(
                    post.queued_messages,
                    pre.queued_messages - drained,
                    "Steered(count={count}) drain mismatch: pre {} → post {} (expected drain {})",
                    pre.queued_messages,
                    post.queued_messages,
                    drained,
                );
            }
        }
        PostCheck::PermissionRequested => {
            // Idle dispatch falls through to the `_ => {}` arm in
            // `handle_idle_engine_event` - no state change. Only enforce
            // the trichotomy when a turn was running.
            if pre.agent_running {
                let new_confirms = post.pending_confirms.saturating_sub(pre.pending_confirms);
                let new_deferred = post.deferred_dialogs.saturating_sub(pre.deferred_dialogs);
                let new_decisions = count_action(new_actions, |c| {
                    matches!(c, UiCommand::PermissionDecision { .. })
                });
                let total = new_confirms + new_deferred + new_decisions;
                assert_eq!(
                    total, 1,
                    "RequestPermission produced {total} effects (confirms={new_confirms}, deferred={new_deferred}, decisions={new_decisions}), expected exactly 1",
                );
            }
        }
        PostCheck::ConfirmResolved {
            approved,
            had_message,
        } => {
            // Side-channel returns false when no confirm was pending -
            // skip in that case.
            if pre.pending_confirms == 0 {
                return;
            }
            assert_eq!(
                post.pending_confirms,
                pre.pending_confirms - 1,
                "Resolve did not remove one confirm: pre {} -> post {}",
                pre.pending_confirms,
                post.pending_confirms,
            );
            let new_decisions = count_action(new_actions, |c| {
                matches!(c, UiCommand::PermissionDecision { .. })
            });
            assert_eq!(
                new_decisions, 1,
                "Resolve did not queue exactly one PermissionDecision (got {new_decisions})",
            );
            // Approve never ends the turn. Deny without a message ends it
            // (resolve_confirm returns true → discard_turn). Deny with a
            // message keeps the turn alive - the user sent steering text
            // instead of stopping.
            if pre.agent_running {
                let should_end = !approved && !had_message;
                if should_end {
                    assert!(
                        !post.agent_running,
                        "Deny without message did not end the turn",
                    );
                } else {
                    assert!(
                        post.agent_running,
                        "{} ended the turn unexpectedly",
                        if approved {
                            "Approve"
                        } else {
                            "Deny with message"
                        },
                    );
                }
            }
        }
        PostCheck::ToolDispatched => {
            if pre.agent_running {
                let new_results =
                    count_action(new_actions, |c| matches!(c, UiCommand::ToolResult { .. }));
                assert!(
                    new_results <= 1,
                    "ToolDispatch should queue at most one ToolResult in-step (got {new_results})",
                );
            }
        }
        PostCheck::ToolEvaluationRequested => {
            if pre.agent_running {
                let new_responses = count_action(new_actions, |c| {
                    matches!(c, UiCommand::ToolEvaluationResponse { .. })
                });
                assert_eq!(
                    new_responses, 1,
                    "ToolEvaluationRequest should queue exactly one ToolEvaluationResponse (got {new_responses})",
                );
            }
        }
        PostCheck::CoreToolResultReceived => {
            // No-op when no pending coroutine matches the request_id.
            // `assert_invariants` covers the no-panic property.
        }
        PostCheck::ShutdownReceived => {
            if pre.agent_running {
                assert!(!post.agent_running, "Shutdown did not end the active turn",);
            }
        }
        PostCheck::TurnCancelled => {
            if pre.agent_running {
                assert!(!post.agent_running, "Cancel did not end the active turn",);
            }
            assert!(
                !post.streaming.text && !post.streaming.thinking,
                "Cancel left streaming active: {:?}",
                post.streaming
            );
        }
        PostCheck::PromptCharInserted {
            ch,
            lua_keymap_bound,
        } => {
            if !pre.prompt_text_input_ready
                || lua_keymap_bound
                || ch.is_control()
                || ch == '\u{FFFC}'
                || (ch == '?' && pre.prompt_source.is_empty())
            {
                return;
            }
            let mut expected = pre.prompt_source.clone();
            let at = smelt_buffer::text::insert(&mut expected, pre.prompt_cpos, ch);
            assert_eq!(
                post.prompt_source, expected,
                "prompt char {ch:?} inserted at wrong location: pre cpos {}, pre {:?}, post {:?}",
                pre.prompt_cpos, pre.prompt_source, post.prompt_source,
            );
            // Text already to the right can join the new char into one grapheme.
            let expected_cpos = smelt_buffer::text::ceil_grapheme(&expected, at + ch.len_utf8());
            assert_eq!(
                post.prompt_cpos, expected_cpos,
                "prompt char {ch:?} left cursor at {}, expected {expected_cpos}",
                post.prompt_cpos,
            );
        }
        PostCheck::PromptTextInserted { text } => {
            if !pre.prompt_paste_target_ready {
                return;
            }
            let inserted = normalize_prompt_paste_text(&text);
            let mut expected = pre.prompt_source.clone();
            let expected_cpos = if let Some((start, end)) = pre.prompt_selection_range {
                smelt_buffer::text::replace_range(&mut expected, start..end, &inserted);
                start + inserted.len()
            } else {
                let at = smelt_buffer::text::insert_str(&mut expected, pre.prompt_cpos, &inserted);
                at + inserted.len()
            };
            let expected_cpos = smelt_buffer::text::ceil_grapheme(&expected, expected_cpos);
            assert_eq!(
                post.prompt_source, expected,
                "prompt paste changed source incorrectly: pre cpos {}, selection {:?}, pasted {:?}, normalized {:?}, pre {:?}, post {:?}",
                pre.prompt_cpos, pre.prompt_selection_range, text, inserted, pre.prompt_source, post.prompt_source,
            );
            assert_eq!(
                post.prompt_cpos, expected_cpos,
                "prompt paste left cursor at {}, expected {} after inserting {:?} from paste {:?}",
                post.prompt_cpos, expected_cpos, inserted, text,
            );
            if matches!(pre.prompt_vim_mode, VimMode::Visual | VimMode::VisualLine)
                && pre.prompt_selection_range.is_some()
            {
                assert_eq!(
                    post.prompt_vim_mode,
                    VimMode::Normal,
                    "prompt paste over {:?} left vim mode at {:?}",
                    pre.prompt_vim_mode,
                    post.prompt_vim_mode,
                );
            }
        }
    }
}

/// Cross-cutting invariants for any op that ends an active turn.
/// `finish_turn` always flushes streaming text + thinking before clearing
/// the agent; if either survives, the flush path was skipped.
pub(crate) fn check_turn_end_invariants(pre: &Snapshot, post: &Snapshot) {
    if pre.agent_running && !post.agent_running {
        assert!(
            !post.streaming.text && !post.streaming.thinking,
            "turn ended with streaming active: {:?}",
            post.streaming
        );
    }
}
