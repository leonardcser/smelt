//! Per-frame render loop: resolves the Lua-composed layout, commits transcript
//! projection and observers, dispatches per-window Lua renderers, synchronizes
//! prompt input, then paints the prepared frame.

use crate::app::TuiApp;
use crate::content::{layout, prompt_buf};

fn is_engine_stream_delta(event: &protocol::EngineEvent) -> bool {
    matches!(
        event,
        protocol::EngineEvent::ReasoningPartDelta { .. }
            | protocol::EngineEvent::TextDelta { .. }
            | protocol::EngineEvent::ToolCallDraftDelta { .. }
            | protocol::EngineEvent::ToolOutput { .. }
            | protocol::EngineEvent::EngineAskDelta { .. }
    )
}

fn starts_or_updates_live_engine_output(event: &protocol::EngineEvent) -> bool {
    is_engine_stream_delta(event)
        || matches!(
            event,
            protocol::EngineEvent::ReasoningPartStarted { .. }
                | protocol::EngineEvent::ToolCallDraftStarted { .. }
                | protocol::EngineEvent::ToolStarted { .. }
        )
}

const STREAMING_FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrameUrgency {
    Streaming,
    Animation(std::time::Duration),
    Urgent,
}

impl FrameUrgency {
    fn interval(self) -> std::time::Duration {
        match self {
            Self::Streaming => STREAMING_FRAME_INTERVAL,
            Self::Animation(interval) => interval,
            Self::Urgent => std::time::Duration::ZERO,
        }
    }

    fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Urgent, _) | (_, Self::Urgent) => Self::Urgent,
            (Self::Streaming, Self::Streaming) => Self::Streaming,
            (Self::Animation(left), Self::Animation(right)) => Self::Animation(left.min(right)),
            (Self::Streaming, Self::Animation(interval))
            | (Self::Animation(interval), Self::Streaming) => {
                Self::Animation(STREAMING_FRAME_INTERVAL.min(interval))
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct PendingFrameTrace {
    id: u64,
    requested_at_us: u64,
    urgency: FrameUrgency,
}

#[derive(Debug)]
pub(crate) struct FrameScheduler {
    pending: Option<PendingFrameTrace>,
    next_id: u64,
    last_frame_at: Option<std::time::Instant>,
}

impl Default for FrameScheduler {
    fn default() -> Self {
        Self {
            pending: None,
            next_id: 1,
            last_frame_at: None,
        }
    }
}

impl FrameScheduler {
    pub(crate) fn request(&mut self, urgency: FrameUrgency) {
        if let Some(pending) = &mut self.pending {
            pending.urgency = pending.urgency.merge(urgency);
            return;
        }
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let requested_at_us = smelt_perf::perf::timestamp_us();
        self.pending = Some(PendingFrameTrace {
            id,
            requested_at_us,
            urgency,
        });
        smelt_perf::perf::record_value("frame:requested:id", id);
        smelt_perf::perf::record_value("frame:requested:at_us", requested_at_us);
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub(crate) fn is_due(&self, now: std::time::Instant) -> bool {
        let Some(pending) = self.pending else {
            return false;
        };
        let interval = pending.urgency.interval();
        interval.is_zero()
            || self
                .last_frame_at
                .is_none_or(|last| now.saturating_duration_since(last) >= interval)
    }

    pub(crate) fn next_delay(&self, now: std::time::Instant) -> Option<std::time::Duration> {
        let pending = self.pending?;
        let interval = pending.urgency.interval();
        Some(
            self.last_frame_at
                .map_or(std::time::Duration::ZERO, |last| {
                    interval.saturating_sub(now.saturating_duration_since(last))
                }),
        )
    }

    fn begin_frame(&mut self, now: std::time::Instant) -> Option<PendingFrameTrace> {
        self.last_frame_at = Some(now);
        self.pending.take()
    }

    fn record_flushed(trace: PendingFrameTrace) {
        let flushed_at_us = smelt_perf::perf::timestamp_us();
        smelt_perf::perf::record_value("frame:flushed:id", trace.id);
        smelt_perf::perf::record_value(
            "frame:request_to_flush:us",
            flushed_at_us.saturating_sub(trace.requested_at_us),
        );
        smelt_perf::perf::record_value("frame:flushed:at_us", flushed_at_us);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EngineOutputDrainOutcome {
    Drained,
    FrameBoundary,
}

pub(super) struct TranscriptSearchProjection<'a> {
    pub(super) anchor: Option<crate::app::transcript::TranscriptSearchRangeAnchor>,
    pub(super) range_after: &'a mut Option<crate::smelt_edit::DocRange>,
}

fn record_transcript_projection_hydration_failure(
    error: crate::app::transcript::TranscriptProjectionHydrationError,
) {
    smelt_perf::perf::record_value(
        "transcript:projection_hydration_failure:required_blocks",
        error.required_blocks as u64,
    );
    smelt_perf::perf::record_value(
        "transcript:projection_hydration_failure:missing_blocks",
        error.missing_blocks as u64,
    );
}

pub(super) fn prepare_transcript_window(
    transcript: &mut crate::app::transcript::TranscriptDocument,
    lua: &smelt_core::lua::runtime::LuaRuntime,
    theme: &std::sync::Arc<crate::smelt_edit::Theme>,
    ui: &mut crate::smelt_edit::Ui,
    request: crate::smelt_edit::MaterializeRequest,
    render_now: std::time::Instant,
    search_projection: TranscriptSearchProjection<'_>,
) {
    if crate::app::document::DocumentRegistry::resolve_optional(request.document_handle)
        != Some(crate::app::document::RegisteredDocument::Transcript)
    {
        return;
    }
    let viewport_rows = request.rect.height;
    let pending_restore = transcript.take_pending_projection_restore();
    let fallback_cursor_screen_row = ui
        .win(request.win)
        .and_then(|win| win.cursor_screen_row(viewport_rows));
    let transcript_scroll_state;
    let transcript_cursor_range;
    let suppress_cursor_screen_row_restore;
    {
        let _p = smelt_perf::perf::begin("compositor:project_transcript");
        let width = request.content_width.max(1);
        let previous_content_width = ui
            .win(request.win)
            .and_then(|win| win.viewport.map(|viewport| viewport.content_width));
        let width_changed = previous_content_width.is_some_and(|previous| previous != width);
        let search_anchor = search_projection.anchor.clone();
        let plan = transcript.plan_viewport_projection_measured(
            lua,
            width,
            theme,
            crate::app::transcript::TranscriptViewportProjectionInput {
                fallback_scroll_top: request.scroll_top,
                follow_tail: request.follow_tail,
                width_changed,
                previous_width: previous_content_width,
            },
            viewport_rows,
        );
        let Some(buf) = ui.buf_mut(request.buf) else {
            return;
        };
        let mut applied = match plan {
            Ok(plan) => transcript.project_applied_viewport(lua, buf, theme, plan),
            Err(error) => {
                record_transcript_projection_hydration_failure(error);
                transcript.project_hydration_failure(buf, viewport_rows)
            }
        };
        let settled_search_range = if let Some(anchor) = search_anchor {
            let matched = transcript.resolve_search_range_anchor(lua, width, theme, anchor.clone());
            let search_needs_projection = width_changed || applied.cursor_range.is_some();
            if search_needs_projection {
                let target_screen_row = applied
                    .cursor_range
                    .map(|range| {
                        range
                            .start
                            .row
                            .saturating_sub(applied.materialized_rows.clamped_scroll)
                    })
                    .unwrap_or_else(|| fallback_cursor_screen_row.unwrap_or_default().into())
                    .min(crate::smelt_edit::RowIndex::from(
                        viewport_rows.saturating_sub(1),
                    ));
                transcript.set_pending_projection_with_hint(
                    crate::app::transcript_scroll_trace::TranscriptScrollIntent::SearchJump {
                        anchor: matched.anchor,
                        target_screen_row,
                        match_start_byte_col: matched.start_byte_col(),
                        match_end_byte_col: matched.end_byte_col(),
                    },
                    crate::app::transcript::TranscriptProjectionRestore::default(),
                    None,
                    Some(
                        crate::app::transcript::TranscriptProjectionHint::SearchProjectedRow {
                            width,
                            anchor: matched.anchor,
                            start_byte_col: matched.start_byte_col(),
                            row: matched.range.start.row,
                            prefer_projected_row: true,
                        },
                    ),
                );
                let search_plan = transcript.plan_viewport_projection_measured(
                    lua,
                    width,
                    theme,
                    crate::app::transcript::TranscriptViewportProjectionInput {
                        fallback_scroll_top: applied.materialized_rows.clamped_scroll,
                        follow_tail: false,
                        width_changed: false,
                        previous_width: None,
                    },
                    viewport_rows,
                );
                match search_plan {
                    Ok(plan) => {
                        applied = transcript.project_applied_viewport(lua, buf, theme, plan);
                    }
                    Err(error) => record_transcript_projection_hydration_failure(error),
                }
                let matched = transcript.resolve_search_range_anchor(lua, width, theme, anchor);
                let range = applied.cursor_range.unwrap_or(matched.range);
                *search_projection.range_after = Some(range);
                Some(range)
            } else {
                None
            }
        } else {
            None
        };
        let desired_scroll_state = applied.scroll_state;
        suppress_cursor_screen_row_restore = settled_search_range.is_some();
        transcript_cursor_range = settled_search_range.or(applied.cursor_range);
        let tdata = applied.materialized_rows;
        debug_assert_eq!(applied.scrollbar_total_rows, tdata.total_rows);
        debug_assert_eq!(applied.exact_visible_range.start, tdata.clamped_scroll);
        debug_assert!(
            applied.exact_visible_range.end <= tdata.total_rows,
            "applied transcript viewport reports an out-of-bounds visible range"
        );
        if applied.placeholder_rows_visible {
            debug_assert!(applied.top_anchor.is_some());
        }
        if let Some(win) = ui.win_mut(request.win) {
            debug_assert!(tdata.total_rows >= tdata.row_base);
            debug_assert!(
                tdata.clamped_scroll <= tdata.total_rows.saturating_sub(viewport_rows as _)
            );
            win.apply_materialized_rows(tdata);
            transcript_scroll_state =
                win.apply_projected_scroll(tdata.clamped_scroll, desired_scroll_state);
        } else {
            transcript_scroll_state = desired_scroll_state;
        }
    }
    let (win, buf) = ui.win_and_buf_mut(request.win, request.buf);
    if let (Some(win), Some(buf)) = (win, buf) {
        let mut restore = crate::smelt_edit::DocumentViewScreenRowRestore {
            cursor: pending_restore.cursor_screen_row,
            cursor_selection: crate::smelt_edit::CursorScreenRowSelection::RestoreActiveSelection,
            drag_endpoint: pending_restore.drag_endpoint_screen_row,
        };
        if suppress_cursor_screen_row_restore {
            restore.cursor = None;
            restore.cursor_selection =
                crate::smelt_edit::CursorScreenRowSelection::SkipActiveSelection;
        } else if restore.cursor.is_none() {
            restore.cursor = fallback_cursor_screen_row;
            restore.cursor_selection =
                crate::smelt_edit::CursorScreenRowSelection::SkipActiveSelection;
        }
        win.restore_document_view_screen_rows(buf, restore);
        if let Some(range) = transcript_cursor_range {
            if win.set_row_cursor(buf, range.start) {
                *search_projection.range_after = Some(range);
            }
        }
        if win.has_materialized_rows() {
            win.sync_yank_flash_layer(buf, viewport_rows, render_now);
            if matches!(
                transcript_scroll_state,
                crate::smelt_edit::VerticalScroll::Tail
            ) {
                win.reveal_row_cursor(buf, viewport_rows);
            }
            win.scroll_left = 0;
        } else {
            let text = buf.text();
            win.clamp_anchors_to_source(&text);
            buf.clear_range_layer(crate::smelt_edit::RangeLayer::Selection);
            win.sync_yank_flash_layer(buf, viewport_rows, render_now);
            win.scroll_left = 0;
        }
    }
}

impl TuiApp {
    fn prepare_committed_transcript_view(
        &mut self,
        focused: bool,
    ) -> Option<crate::smelt_edit::PreparedWindowRequest> {
        let theme = self.ui.theme().clone();
        let render_now = self.core.clock.instant_now();
        let transcript_search_anchor = self
            .overlays
            .search_session()
            .filter(|session| session.target == crate::app::TRANSCRIPT_WIN)
            .and_then(|session| match &session.backend {
                crate::app::search::SearchBackend::Transcript(transcript) => transcript
                    .current
                    .and_then(|index| transcript.matches.get(index).copied())
                    .map(|matched| (matched, session.query.clone())),
                crate::app::search::SearchBackend::Full { .. } => None,
            })
            .map(|(matched, query)| {
                self.conversation
                    .transcript_search_range_anchor(matched, query)
            });
        let mut transcript_search_range_after_projection = None;
        let lua = self.lua.execution();
        let prepared_transcript = {
            let core = &mut self.core;
            let conversation = &mut self.conversation;
            let ui = &mut self.ui;
            smelt_core::host::scope_core(core, || {
                ui.prepare_split_window_with(crate::app::TRANSCRIPT_WIN, |ui, request| {
                    conversation.prepare_transcript_window(
                        &lua,
                        &theme,
                        ui,
                        request,
                        render_now,
                        TranscriptSearchProjection {
                            anchor: transcript_search_anchor.clone(),
                            range_after: &mut transcript_search_range_after_projection,
                        },
                    );
                })
            })
        };
        let transcript_visible = prepared_transcript.is_some();
        if let Some(range) = transcript_search_range_after_projection {
            self.update_current_transcript_search_range(crate::app::TRANSCRIPT_WIN, range);
        }
        self.capture_committed_transcript_view(focused && transcript_visible, transcript_visible);
        self.dispatch_committed_transcript_view();
        prepared_transcript
    }

    fn capture_committed_transcript_view(&mut self, focused: bool, visible: bool) {
        let (width, height, content_width, scrollable, following_tail, at_top, at_bottom, cursor) = {
            let win = self.transcript_win();
            let viewport = visible.then_some(win.viewport).flatten();
            let width = viewport.map(|viewport| viewport.rect.width).unwrap_or(0);
            let height = viewport.map(|viewport| viewport.rect.height).unwrap_or(0);
            let content_width = viewport.map(|viewport| viewport.content_width).unwrap_or(0);
            let total = self
                .ui
                .buf(win.buf)
                .map(|buf| win.scroll_row_total(buf))
                .unwrap_or(0);
            let max = total.saturating_sub(height as u64);
            let top = win.scroll_top().min(max);
            let cursor = if focused {
                win.cursor_screen_row(height)
            } else {
                None
            };
            (
                width,
                height,
                content_width,
                viewport.is_some() && total > height as u64,
                win.is_following_tail(),
                viewport.is_some() && top == 0,
                viewport.is_some() && top >= max,
                cursor,
            )
        };
        let state = crate::app::TranscriptViewState {
            session_id: self.conversation.session().id.clone(),
            navigation_generation: self
                .conversation
                .transcript()
                .history()
                .navigation_generation(),
            anchor: self.conversation.transcript().current_navigation_anchor(),
            width,
            height,
            content_width,
            scrollable,
            following_tail,
            at_top,
            at_bottom,
            focused,
            cursor_viewport_row: cursor,
        };
        self.conversation.commit_transcript_view(state);
    }

    fn dispatch_committed_transcript_view(&mut self) {
        let Some(view) = self.conversation.committed_transcript_view() else {
            return;
        };
        let callbacks = self
            .lua
            .shared()
            .transcript_view_watchers
            .pending_callbacks(self.lua.lua(), view.revision);
        let prepared: Vec<(mlua::Function, mlua::AnyUserData)> = callbacks
            .into_iter()
            .filter_map(|callback| {
                self.lua
                    .lua()
                    .create_userdata(crate::lua::api::transcript::LuaTranscriptView::new(
                        view.clone(),
                    ))
                    .ok()
                    .map(|payload| (callback, payload))
            })
            .collect();
        let lua = self.lua.execution();
        crate::lua::scope_app(self, move || {
            for (callback, payload) in prepared {
                if let Err(error) = callback.call::<()>(payload) {
                    lua.record_error(format!("transcript view watcher: {error}"));
                }
            }
        });
    }

    pub(crate) fn render_normal(&mut self) {
        let mut stdout = std::io::stdout();
        self.render_normal_to(&mut stdout);
    }

    pub(crate) fn render_frame_to<W: std::io::Write>(&mut self, out: &mut W) {
        self.publish_diff_signals();
        self.render_normal_to(out);
    }

    /// Drain one bounded batch from a continuously-ready engine output queue.
    /// `FrameBoundary` tells the caller to paint before processing more output.
    pub(crate) fn drain_ready_engine_outputs_for_frame_to<
        W: std::io::Write,
        F: FnMut(&mut Self),
    >(
        &mut self,
        out: &mut W,
        mut on_transient_frame: F,
    ) -> EngineOutputDrainOutcome {
        let drain_started_at = std::time::Instant::now();
        let mut drained_outputs = 0;
        while drained_outputs < crate::app::READY_QUEUE_DRAIN_MAX_ITEMS_PER_FRAME
            && (drained_outputs == 0
                || drain_started_at.elapsed() < crate::app::READY_QUEUE_DRAIN_MAX_DURATION)
        {
            let output = match self.core.engine.try_recv_output() {
                Ok(output) => output,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    return EngineOutputDrainOutcome::Drained;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    engine::log::entry(
                        engine::log::Level::Warn,
                        "engine_stop",
                        &serde_json::json!({
                            "reason": "channel_disconnected",
                            "source": "try_recv_drain",
                        }),
                    );
                    self.discard_turn(crate::app::TurnEnd::Errored {
                        kind: None,
                        retry_at_ms: None,
                    });
                    return EngineOutputDrainOutcome::Drained;
                }
            };
            drained_outputs += 1;
            if !self.dispatch_engine_output_in_render_loop_to(output, out, |app| {
                on_transient_frame(app)
            }) {
                return EngineOutputDrainOutcome::FrameBoundary;
            }
        }
        EngineOutputDrainOutcome::FrameBoundary
    }

    pub(crate) fn drain_ready_engine_outputs_for_frame(&mut self) -> EngineOutputDrainOutcome {
        self.drain_ready_engine_outputs_for_frame_to(&mut std::io::stdout(), |_| {})
    }

    pub(crate) fn render_requested_transient_frame_to<W: std::io::Write>(
        &mut self,
        out: &mut W,
    ) -> bool {
        if !self.scheduled_frame_is_due() {
            return false;
        }
        self.render_frame_to(out);
        true
    }

    pub(crate) fn render_pending_transient_frame_to<W: std::io::Write>(
        &mut self,
        out: &mut W,
    ) -> bool {
        if !self.frame_scheduler.has_pending() {
            return false;
        }
        self.render_frame_to(out);
        true
    }

    pub(crate) fn dispatch_engine_output_in_render_loop(
        &mut self,
        output: engine::EngineOutput,
    ) -> bool {
        self.dispatch_selected_engine_output_in_render_loop_to(output, &mut std::io::stdout())
    }

    pub(crate) fn dispatch_selected_engine_output_in_render_loop_to<W: std::io::Write>(
        &mut self,
        output: engine::EngineOutput,
        out: &mut W,
    ) -> bool {
        let keep_streaming = self.dispatch_engine_output_in_render_loop_to(output, out, |_| {});
        // Selected outputs bypass the ready-queue drain, so honor render
        // requests raised during dispatch before waiting for more input.
        self.render_requested_transient_frame_to(out);
        keep_streaming
    }

    pub(crate) fn dispatch_engine_output_in_render_loop_to<
        W: std::io::Write,
        F: FnOnce(&mut Self),
    >(
        &mut self,
        output: engine::EngineOutput,
        out: &mut W,
        on_transient_frame: F,
    ) -> bool {
        match output {
            engine::EngineOutput::Event(event) => {
                self.dispatch_engine_event_in_render_loop_to(event, out, on_transient_frame)
            }
            engine::EngineOutput::HostCall(call) => {
                if self.render_requested_transient_frame_to(out) {
                    on_transient_frame(self);
                }
                self.dispatch_host_call(call);
                true
            }
        }
    }

    pub(crate) fn dispatch_engine_event_in_render_loop_to<
        W: std::io::Write,
        F: FnOnce(&mut Self),
    >(
        &mut self,
        ev: protocol::EngineEvent,
        out: &mut W,
        on_transient_frame: F,
    ) -> bool {
        if self.render_transient_frame_before_engine_event_to(&ev, out) {
            on_transient_frame(self);
        }
        let updates_live_output = starts_or_updates_live_engine_output(&ev);
        let coalescible_continuation = matches!(
            &ev,
            protocol::EngineEvent::ReasoningPartDelta { .. }
                | protocol::EngineEvent::TextDelta { .. }
        ) && self.conversation.has_live_transcript_blocks();
        let keep_streaming = self.dispatch_engine_event(ev);
        if updates_live_output && coalescible_continuation {
            self.request_streaming_render();
        } else {
            self.request_urgent_render();
        }
        keep_streaming
    }

    pub(crate) fn render_transient_frame_before_engine_event_to<W: std::io::Write>(
        &mut self,
        ev: &protocol::EngineEvent,
        out: &mut W,
    ) -> bool {
        if is_engine_stream_delta(ev) {
            return false;
        }
        self.render_pending_transient_frame_to(out)
    }

    pub(crate) fn refresh_main_layout(&mut self) -> (layout::Rect, u16) {
        if smelt_core::host::host_access_active() {
            self.lua.shared().request_layout_refresh();
            return (self.layout.prompt, self.layout.viewport_rows());
        }
        let applying_deferred_layout = self.lua.shared().take_layout_refresh();
        let (term_w, term_h) = self.ui.terminal_size();
        let width = term_w as usize;
        let ghost = self.prompt.placeholder_text(crate::app::PROMPT_WIN);
        let wrapped_rows = self.measure_prompt_input_rows(self.prompt_buf(), width, ghost);
        // Auto-height keeps the transcript usable; a deliberate manual resize
        // can claim more room for prompt review without taking the full screen.
        let input_rows = self.prompt.resolve_height(wrapped_rows, term_h);
        let tree = self
            .invoke_lua_layout_composer(term_w, term_h, input_rows)
            .unwrap_or_else(|| self.fallback_main_layout(term_w, term_h, input_rows));
        self.ui.set_layout(tree);
        self.layout = layout::LayoutState::from_ui(&self.ui);
        // Prompt-docked pickers size themselves to the headroom above the
        // prompt chrome; recompute them whenever the main layout changes.
        crate::picker::sync_layouts(self);
        if applying_deferred_layout {
            if let Some(focus) = self.lua.shared().take_focus_after_layout() {
                self.focus_window(focus);
            }
            if self.ui.focus().is_none() && self.ui.active_modal().is_none() {
                let focus = match self.app_focus {
                    crate::app::AppFocus::Prompt => crate::app::PROMPT_WIN,
                    crate::app::AppFocus::Content => crate::app::TRANSCRIPT_WIN,
                };
                self.focus_window(focus);
            }
        }
        let prompt_rect = if self.has_docked_dialog() {
            // Keep the hidden prompt's parser/cursor projection current without
            // mounting it in the root layout. This makes restoration lossless.
            layout::Rect::new(0, 0, term_w, input_rows)
        } else {
            self.layout.prompt
        };
        (prompt_rect, self.layout.viewport_rows())
    }

    /// Render variant parameterised by the output sink. Production passes
    /// `std::io::stdout()`; the fuzz harness passes `std::io::sink()` so
    /// every code path under `content/*` and `compositor:*` runs without
    /// dumping megabytes of ANSI per scenario into libFuzzer's log file.
    pub(crate) fn render_normal_to<W: std::io::Write>(&mut self, out: &mut W) {
        let frame_trace = self
            .frame_scheduler
            .begin_frame(self.core.clock.instant_now());
        let _perf = smelt_perf::perf::begin("app:tick_compositor");
        self.update_spinner();

        let show_queued = self.prompt_input_is_busy();

        self.ui.resolve_tail_scrolls();
        self.ui.sync_scroll_links();

        let queued_owned: Vec<String> = if show_queued {
            self.prompt.queued_texts()
        } else {
            Vec::new()
        };
        let queued: &[String] = &queued_owned;

        let (has_prompt_cursor, has_transcript_cursor) = self.compute_cursor_ownership();

        // Hidden is the right baseline; sync paths below set Block when focus owns the caret.
        self.ui
            .set_cursor_shape(crate::smelt_edit::CursorShape::Hidden);

        // ── Layout ──
        let (prompt_rect, _viewport_rows) = {
            let _p = smelt_perf::perf::begin("compositor:layout");
            self.refresh_main_layout()
        };

        // Freeze timer/spinner while a blocking dialog is up. Done before
        // Lua renderers run so the prompt top-bar indicator they paint
        // this frame already reflects the pause.
        self.set_agent_blocked_paused(self.ui.active_modal_blocks_agent());
        let now = self.core.clock.instant_now();
        self.conversation.sync_active_tool_elapsed(now);
        self.sync_transcript_renderer_generation();

        // Commit transcript projection before Lua observers run. Plugins receive
        // one coherent view snapshot and can update overlays for this same frame.
        let prepared_transcript = self.prepare_committed_transcript_view(has_transcript_cursor);

        {
            let _p = smelt_perf::perf::begin("compositor:lua_renderers");
            self.dispatch_lua_renderers();
        }
        // Suppress unused-variable warning when queued is only forwarded into Lua state.
        let _ = queued;
        // Row-backed drag state uses absolute document rows, so the committed
        // backing slice can follow edge autoscroll without moving its anchor.
        {
            let _p = smelt_perf::perf::begin("compositor:input");
            self.sync_input_layer(prompt_rect, has_prompt_cursor);
        }

        if has_transcript_cursor {
            self.ui
                .set_cursor_shape(prompt_block_cursor(self.ui.theme()));
        }

        self.finalize_layer_rects();

        // Late cursor-shape fill-ins. Each sync layer above sets `cursor_shape` for
        // the focus context it owns (transcript / prompt). Two cross-cutting cases
        // are decided here, after the layers have spoken, by forcing `Block` only
        // if no layer has already claimed the cursor:
        //   - Focused overlay leaf (dialog / picker) - leaf's own `cursor_screen_row`
        //     paints the block via `Window::render`.
        //   - Active mouse drag anywhere - `Ui::active_cursor_leaf` routes the block
        //     to the dragging leaf so the cursor visibly follows the drag, even on a
        //     non-focusable leaf like a notification.
        if matches!(
            self.ui.cursor_shape(),
            crate::smelt_edit::CursorShape::Hidden
        ) {
            let transient_focus =
                self.ui.focused_overlay().is_some() || self.ui.focused_modal().is_some();
            if transient_focus || self.ui.any_drag_active() {
                self.ui
                    .set_cursor_shape(prompt_block_cursor(self.ui.theme()));
            }
        }

        let _p = smelt_perf::perf::begin("compositor:render_flush");

        // Transcript content was materialized by the committed-view phase. The
        // prepare pass still materializes other row-backed windows and applies search.
        let prepared_frame = {
            let overlays = &mut self.overlays;
            let render_now = self.core.clock.instant_now();
            let search_session = overlays
                .search_session()
                .map(crate::app::search::SearchRenderSession::from);
            self.ui.prepare_frame_with_prepared_splits(
                prepared_transcript,
                |ui, request| {
                    let width = request.content_width as usize;
                    if let Some((kind, summary)) =
                        overlays.notification_needs_render(request.win, width)
                    {
                        crate::app::TuiApp::write_notification_buf(
                            ui,
                            request.buf,
                            kind,
                            &summary,
                            width,
                        );
                    }
                },
                |ui, request| {
                    let (win, buf) = ui.win_and_buf_mut(request.win, request.buf);
                    let (Some(win), Some(buf)) = (win, buf) else {
                        return;
                    };
                    if !win.has_materialized_rows() {
                        win.sync_yank_flash_layer(buf, request.rect.height, render_now);
                    }
                    win.clear_range_layer(crate::smelt_edit::RangeLayer::Search);
                    if let Some(search) =
                        search_session.as_ref().filter(|s| s.target == request.win)
                    {
                        search.apply_to_window(win, buf, request.rect.height);
                    }
                },
            )
        };

        // Lua paint callbacks need a scoped app host, which cannot alias the UI
        // borrow held by the compositor. Render them into transparent layers from
        // the prepared frame, then insert each layer by its stable painter slot.
        let paint_jobs: Vec<_> = if self.paint_registry.is_empty() {
            Vec::new()
        } else {
            prepared_frame
                .paint_leaves()
                .iter()
                .enumerate()
                .filter_map(|(slot, leaf)| {
                    self.paint_registry
                        .lookup(leaf.id)
                        .map(|handle| (slot, handle))
                })
                .collect()
        };
        let mut prepared_paints: Vec<Option<crate::lua::paint::PaintLayer>> =
            std::iter::repeat_with(|| None)
                .take(prepared_frame.paint_leaves().len())
                .collect();
        if !paint_jobs.is_empty() {
            let lua = self.lua.execution();
            crate::lua::scope_app(self, || {
                for (slot, handle) in paint_jobs {
                    let leaf = &prepared_frame.paint_leaves()[slot];
                    prepared_paints[slot] = Some(crate::lua::paint::render_paint(
                        &lua,
                        handle,
                        leaf.rect.width,
                        leaf.rect.height,
                        &leaf.context,
                    ));
                }
            });
        }

        let frame = self.ui.paint_prepared_frame_with_paints(
            prepared_frame,
            |slot, _id, slice, _context| {
                if let Some(layer) = prepared_paints[slot].take() {
                    layer.composite_into(slice);
                }
            },
        );
        let _ = self.ui.flush_prepared_frame(out, frame);
        if let Some(trace) = frame_trace {
            FrameScheduler::record_flushed(trace);
        }
    }

    /// Compute which pane owns the cursor this frame.
    /// Cmdline/modal/overlay focus steals it; terminal-unfocused suppresses it.
    fn compute_cursor_ownership(&self) -> (bool, bool) {
        let transient_ui_owns_cursor =
            self.ui.focused_overlay().is_some() || self.ui.focused_modal().is_some();
        let cmdline_active = self.well_known.cmdline.is_some();
        let suppress = cmdline_active || transient_ui_owns_cursor;
        let has_prompt_cursor = !suppress
            && self.platform.terminal_is_focused()
            && matches!(self.app_focus, crate::app::AppFocus::Prompt);
        let has_transcript_cursor = !suppress
            && self.platform.terminal_is_focused()
            && matches!(self.app_focus, crate::app::AppFocus::Content);
        (has_prompt_cursor, has_transcript_cursor)
    }

    /// Populate the input-leaf buffer, cursor, and viewport. Cursor positions are content-local;
    /// the leaf's gutter shift is applied by `Window::render`.
    fn sync_input_layer(&mut self, prompt_rect: crate::smelt_edit::Rect, has_prompt_cursor: bool) {
        let gutters = self
            .ui
            .win(crate::app::PROMPT_WIN)
            .map(|w| w.config.gutters)
            .unwrap_or_default();
        // Use the same content width the auto-attach pre-pass will use - both pad
        // gutters AND the reserved scrollbar column. PromptBufferParser now
        // emits unwrapped display rows; Window::ensure_layout owns wrapping at
        // this exact width so cursor projection and paint agree.
        let content_width = gutters.content_width(prompt_rect.width);

        {
            let now = self.core.clock.instant_now();
            let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
            pctx.buf.ensure_rendered_at(content_width);
            let inp = prompt_buf::InputLeafInput {
                input: self.prompt.render_input(),
                win: pctx.win,
                clipboard: &self.core.clipboard,
                now,
            };
            prompt_buf::sync_prompt_overlays(&inp, pctx.buf);
            pctx.win.ensure_layout(pctx.buf, content_width);
            self.prompt
                .sync_display_coords(&mut pctx, prompt_rect.height);
            pctx.win.scroll_left = 0;
            pctx.win.pending_recenter = false;
            pctx.win.set_last_render_cpos(Some(pctx.win.cpos()));
        }

        if has_prompt_cursor {
            let screen_row = self.prompt_win().cursor_screen_row(prompt_rect.height);
            if screen_row.is_some() {
                self.ui
                    .set_cursor_shape(prompt_block_cursor(self.ui.theme()));
            } else {
                // Cursor is off-screen - hide it so a stale shape from the prior frame
                // doesn't draw a stray glyph.
                self.ui
                    .set_cursor_shape(crate::smelt_edit::CursorShape::Hidden);
            }
        }
    }

    fn fallback_main_layout(
        &mut self,
        _term_w: u16,
        _term_h: u16,
        prompt_input_rows: u16,
    ) -> crate::smelt_edit::LayoutTree {
        let content = if let Some(id) = self.active_docked_dialog() {
            self.docked_dialog_stage_layout(id)
                .unwrap_or_else(|| crate::smelt_edit::LayoutTree::leaf(crate::app::TRANSCRIPT_WIN))
        } else {
            layout::seed_layout_tree(prompt_input_rows)
        };

        let Some(statusline) = self.ui.named_win("smelt.statusline") else {
            return content;
        };
        crate::smelt_edit::LayoutTree::vbox(vec![
            (crate::smelt_edit::Constraint::Fill, content),
            (
                crate::smelt_edit::Constraint::Length(1),
                crate::smelt_edit::LayoutTree::leaf(statusline),
            ),
        ])
    }

    /// Invoke the Lua main-layout composer if one is registered via
    /// `smelt.ui.layout.set(fn)`. Returns `None` when no composer is
    /// registered, the resolved function is missing/invalid, the
    /// callback errors, or the returned userdata isn't a `LuaUiLayout`.
    /// [`Self::fallback_main_layout`] runs in any `None` case so the screen
    /// stays usable when a plugin is buggy.
    fn invoke_lua_layout_composer(
        &mut self,
        term_w: u16,
        term_h: u16,
        prompt_input_rows: u16,
    ) -> Option<crate::smelt_edit::LayoutTree> {
        let dialog = self.active_docked_dialog();
        let lua = self.lua.lua();
        let shared = self.lua.shared();
        let composer_func: Option<mlua::Function> = {
            let guard = shared.main_layout_composer.lock().ok()?;
            let handle = guard.as_ref()?;
            lua.registry_value::<mlua::Function>(&handle.key).ok()
        };
        let func = composer_func?;
        let state = lua.create_table().ok()?;
        let _ = state.set("term_w", term_w);
        let _ = state.set("term_h", term_h);
        let _ = state.set("prompt_input_rows", prompt_input_rows);
        if let Some(id) = dialog {
            let layout = lua
                .create_userdata(crate::lua::api::overlay_layout::LuaUiLayout(
                    crate::lua::api::overlay_layout::LayoutNode::DialogStage { id },
                ))
                .ok()?;
            let _ = state.set("dialog", layout);
        }
        let result: mlua::Result<mlua::AnyUserData> =
            crate::lua::scope_app(self, move || func.call((state,)));
        let ud = match result {
            Ok(ud) => ud,
            Err(e) => {
                self.record_lua_error(format!("smelt.ui.layout composer: {e}"));
                return None;
            }
        };
        let node = ud
            .borrow::<crate::lua::api::overlay_layout::LuaUiLayout>()
            .ok()?
            .0
            .clone();
        let (active_stage_count, total_stage_count) = node.dialog_stage_counts(dialog);
        match dialog {
            Some(_) if active_stage_count != 1 || total_stage_count != 1 => {
                self.record_lua_error(format!(
                    "smelt.ui.layout composer must include only the active dialog stage exactly once (found {active_stage_count} active, {total_stage_count} total); using the safe fallback layout"
                ));
                return None;
            }
            None if total_stage_count != 0 => {
                self.record_lua_error(
                    "smelt.ui.layout composer included a dialog stage when no dialog is active; using the safe fallback layout"
                        .into(),
                );
                return None;
            }
            Some(_) | None => {}
        }

        let mut window_leaves: Vec<crate::smelt_edit::WinId> = Vec::new();
        match crate::lua::ui_ops::build_layout_tree(self, &node, &mut window_leaves) {
            Ok((_constraint, tree)) => Some(tree),
            Err(e) => {
                self.record_lua_error(format!("smelt.ui.layout composer tree: {e}"));
                None
            }
        }
    }

    /// Invoke every Lua renderer registered via `Win:set_renderer(fn)`.
    /// Each callback receives its `Win` userdata; the renderer is
    /// expected to write the window's contents into the backing buffer
    /// for the current frame. Renderers whose target window has been
    /// closed are silently skipped (and not collected - `Win:close()`
    /// is the right way to drop a renderer, and the registry entry
    /// stays so a re-opened window keeps its renderer). Errors are
    /// recorded so plugin bugs surface in `/log` without breaking the
    /// frame.
    fn dispatch_lua_renderers(&mut self) {
        let lua = self.lua.lua();
        let shared = self.lua.shared();
        // Snapshot (win_id, function) pairs so the registry mutex
        // isn't held across Lua calls (renderers may legitimately
        // re-register or remove themselves mid-frame).
        let entries: Vec<(crate::smelt_edit::WinId, mlua::Function)> = {
            let guard = match shared.win_renderers.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            guard
                .iter()
                .filter_map(|(raw_id, handle)| {
                    lua.registry_value::<mlua::Function>(&handle.key)
                        .ok()
                        .map(|f| (crate::smelt_edit::WinId(*raw_id), f))
                })
                .collect()
        };
        for (win_id, func) in entries {
            // Skip windows that no longer exist (e.g. closed overlay leaves
            // whose renderer hasn't been cleared yet).
            if self.ui.win(win_id).is_none() {
                continue;
            }
            let win_ud = crate::lua::api::win::LuaWin { id: win_id };
            let result = crate::lua::scope_app(self, move || func.call::<()>((win_ud,)));
            if let Err(e) = result {
                self.record_lua_error(format!("win renderer for {win_id:?}: {e}"));
            }
        }
    }

    fn finalize_layer_rects(&mut self) {
        // Re-assert app-pane focus only when no transient surface owns focus.
        if self.ui.focused_overlay().is_none() && self.ui.active_modal().is_none() {
            match self.app_focus {
                crate::app::AppFocus::Prompt => {
                    self.ui.set_focus(crate::app::PROMPT_WIN);
                }
                crate::app::AppFocus::Content => {
                    self.ui.set_focus(crate::app::TRANSCRIPT_WIN);
                }
            }
        }
    }
}

fn prompt_block_cursor(theme: &crate::smelt_edit::Theme) -> crate::smelt_edit::CursorShape {
    let (fg, bg) = if theme.is_light() {
        (
            smelt_core::style::Color::White,
            smelt_core::style::Color::Black,
        )
    } else {
        (
            smelt_core::style::Color::Black,
            smelt_core::style::Color::White,
        )
    };
    crate::smelt_edit::CursorShape::Block {
        glyph: ' ',
        style: crate::smelt_edit::Style {
            fg: Some(fg),
            bg: Some(bg),
            ..Default::default()
        },
        pos: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_requests_wait_for_frame_interval() {
        let start = std::time::Instant::now();
        let mut scheduler = FrameScheduler::default();
        scheduler.begin_frame(start);
        scheduler.request(FrameUrgency::Streaming);

        assert!(!scheduler.is_due(start + std::time::Duration::from_millis(15)));
        assert_eq!(
            scheduler.next_delay(start + std::time::Duration::from_millis(15)),
            Some(std::time::Duration::from_millis(1))
        );
        assert!(scheduler.is_due(start + STREAMING_FRAME_INTERVAL));
    }

    #[test]
    fn urgent_request_promotes_pending_streaming_frame() {
        let start = std::time::Instant::now();
        let mut scheduler = FrameScheduler::default();
        scheduler.begin_frame(start);
        scheduler.request(FrameUrgency::Streaming);
        scheduler.request(FrameUrgency::Urgent);

        assert!(scheduler.is_due(start + std::time::Duration::from_millis(2)));
        let trace = scheduler
            .begin_frame(start + std::time::Duration::from_millis(2))
            .expect("promoted frame");
        assert_eq!(trace.id, 1);
        assert_eq!(trace.urgency, FrameUrgency::Urgent);
    }

    #[test]
    fn animation_and_streaming_share_earliest_deadline() {
        let start = std::time::Instant::now();
        let mut scheduler = FrameScheduler::default();
        scheduler.begin_frame(start);
        scheduler.request(FrameUrgency::Animation(std::time::Duration::from_millis(
            40,
        )));
        scheduler.request(FrameUrgency::Streaming);

        assert!(!scheduler.is_due(start + std::time::Duration::from_millis(15)));
        assert!(scheduler.is_due(start + STREAMING_FRAME_INTERVAL));
    }

    #[test]
    fn repeated_streaming_requests_coalesce_into_one_trace() {
        let start = std::time::Instant::now();
        let mut scheduler = FrameScheduler::default();
        scheduler.request(FrameUrgency::Streaming);
        for _ in 1..100 {
            scheduler.request(FrameUrgency::Streaming);
        }

        let trace = scheduler.begin_frame(start).expect("coalesced frame");
        assert_eq!(trace.id, 1);
        assert!(!scheduler.has_pending());
    }
}
