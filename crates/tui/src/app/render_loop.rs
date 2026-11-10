//! Per-frame render loop: resolves the Lua-composed layout, commits transcript
//! projection and observers, dispatches per-window Lua renderers, synchronizes
//! prompt input, then paints the prepared frame.

use crate::app::TuiApp;
use crate::content::{layout, prompt_buf};

pub(super) fn is_engine_continuation(event: &protocol::EngineEvent) -> bool {
    matches!(
        event,
        protocol::EngineEvent::ReasoningPartDelta { .. }
            | protocol::EngineEvent::TextDelta { .. }
            | protocol::EngineEvent::ToolCallDraftDelta { .. }
            | protocol::EngineEvent::ToolOutput { .. }
            | protocol::EngineEvent::EngineAskDelta { .. }
    )
}

pub(crate) const CONTINUATION_FRAME_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(16);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrameUrgency {
    /// The next frame on the frame grid.
    Continuation,
    /// A frame at this instant, or on the frame grid if that is later.
    Animation(std::time::Instant),
    Urgent,
}

impl FrameUrgency {
    /// When the frame is due after a frame at `last`; `None` is now.
    fn due_at(self, last: Option<std::time::Instant>) -> Option<std::time::Instant> {
        let grid = last.map(|last| last + CONTINUATION_FRAME_INTERVAL);
        match self {
            Self::Urgent => None,
            Self::Continuation => grid,
            Self::Animation(at) => Some(grid.map_or(at, |grid| grid.max(at))),
        }
    }

    fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Urgent, _) | (_, Self::Urgent) => Self::Urgent,
            (Self::Animation(left), Self::Animation(right)) => Self::Animation(left.min(right)),
            // The grid is never later than an animation deadline clamped to it.
            (Self::Continuation, _) | (_, Self::Continuation) => Self::Continuation,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct PendingFrameTrace {
    id: u64,
    requested_at_us: u64,
    urgency: FrameUrgency,
    /// Streamed output is waiting to be painted.
    streamed: bool,
    /// UI state changed, so the frame is composed again. Otherwise only
    /// present-time effects changed and the last composed frame is reused.
    compose: bool,
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
        self.request_pending(urgency, false, true);
    }

    /// Request a frame that only presents again: present-time effects such
    /// as frame paints and the stream reveal changed, and nothing they draw
    /// over did.
    pub(crate) fn request_present(&mut self, urgency: FrameUrgency) {
        self.request_pending(urgency, false, false);
    }

    /// Request a frame that paints streamed output: engine deltas or queued
    /// transcript work. Such a frame is painted before the next engine event,
    /// which may finish or replace that output.
    pub(crate) fn request_stream(&mut self) {
        self.request_pending(FrameUrgency::Continuation, true, true);
    }

    fn request_pending(&mut self, urgency: FrameUrgency, streamed: bool, compose: bool) {
        if let Some(pending) = &mut self.pending {
            pending.urgency = pending.urgency.merge(urgency);
            pending.streamed |= streamed;
            pending.compose |= compose;
            return;
        }
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let requested_at_us = smelt_perf::perf::timestamp_us();
        self.pending = Some(PendingFrameTrace {
            id,
            requested_at_us,
            urgency,
            streamed,
            compose,
        });
        smelt_perf::perf::record_value("frame:requested:id", id);
        smelt_perf::perf::record_value("frame:requested:at_us", requested_at_us);
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub(crate) fn has_pending_stream(&self) -> bool {
        self.pending.is_some_and(|pending| pending.streamed)
    }

    pub(crate) fn is_due(&self, now: std::time::Instant) -> bool {
        let Some(pending) = self.pending else {
            return false;
        };
        pending
            .urgency
            .due_at(self.last_frame_at)
            .is_none_or(|due| now >= due)
    }

    pub(crate) fn next_delay(&self, now: std::time::Instant) -> Option<std::time::Duration> {
        let pending = self.pending?;
        Some(
            pending
                .urgency
                .due_at(self.last_frame_at)
                .map_or(std::time::Duration::ZERO, |due| {
                    due.saturating_duration_since(now)
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
    pub(super) match_after: &'a mut Option<crate::app::transcript::TranscriptSearchMatch>,
}

fn record_transcript_projection_hydration_deferred(
    error: crate::app::transcript::TranscriptProjectionHydrationError,
) {
    use crate::app::transcript::{
        TranscriptProjectionHydrationError, TranscriptProjectionHydrationPending,
    };

    match error {
        TranscriptProjectionHydrationError::MissingBlocks {
            required_blocks,
            missing_blocks,
        } => {
            smelt_perf::perf::record_value(
                "transcript:projection_hydration_failure:required_blocks",
                required_blocks as u64,
            );
            smelt_perf::perf::record_value(
                "transcript:projection_hydration_failure:missing_blocks",
                missing_blocks as u64,
            );
        }
        TranscriptProjectionHydrationError::Pending(pending) => {
            let metric = match pending {
                TranscriptProjectionHydrationPending::CommandTarget => {
                    "transcript:projection_hydration_pending:command_target"
                }
                TranscriptProjectionHydrationPending::LocalDelta => {
                    "transcript:projection_hydration_pending:local_delta"
                }
                TranscriptProjectionHydrationPending::PreserveViewport => {
                    "transcript:projection_hydration_pending:preserve_viewport"
                }
                TranscriptProjectionHydrationPending::TailRecordWindow => {
                    "transcript:projection_hydration_pending:tail_record_window"
                }
                TranscriptProjectionHydrationPending::FarSeek => {
                    "transcript:projection_hydration_pending:far_seek"
                }
            };
            smelt_perf::perf::record_value(metric, 1);
        }
    }
}

fn transcript_search_range_matches_buffer(
    buf: &crate::smelt_edit::Buffer,
    materialized: crate::smelt_edit::MaterializedRows,
    range: crate::smelt_edit::DocRange,
    query: &str,
) -> bool {
    if range.start.row != range.end.row {
        return false;
    }
    if !materialized.contains_abs_row(range.start.row) {
        return false;
    }
    let local_row = materialized.local_row(range.start.row);
    let Some(line) = buf.get_line(crate::smelt_edit::row_to_usize(local_row)) else {
        return false;
    };
    smelt_buffer::text::slice(line, range.start.byte_col..range.end.byte_col)
        == crate::smelt_edit::SearchPattern::parse(query).needle
}

fn sync_retained_transcript_window(
    ui: &mut crate::smelt_edit::Ui,
    request: crate::smelt_edit::MaterializeRequest,
) {
    let (win, buf) = ui.win_and_buf_mut(request.win, request.buf);
    let (Some(win), Some(buf)) = (win, buf) else {
        return;
    };
    if win.has_materialized_rows() {
        let viewport_rows = win
            .materialized_rows()
            .map_or(request.rect.height, |materialized| {
                if materialized.covers_viewport(win.scroll_top(), request.rect.height) {
                    request.rect.height
                } else {
                    win.retain_prepared_viewport_height()
                        .unwrap_or(request.rect.height)
                        .min(request.rect.height)
                }
            });
        if win.is_following_tail() {
            win.reveal_row_cursor(buf, viewport_rows);
        }
        if let Some(materialized) = win.materialized_rows() {
            if let Some(retained_scroll) =
                materialized.nearest_backed_scroll_top(win.scroll_top(), viewport_rows)
            {
                if retained_scroll != win.scroll_top() {
                    win.set_resolved_scroll(retained_scroll);
                }
            }
            debug_assert!(
                materialized.covers_viewport(win.scroll_top(), viewport_rows),
                "retained transcript viewport {:?} escaped exact backing {:?} (projected_scroll={}, following_tail={})",
                materialized.viewport_range(win.scroll_top(), viewport_rows),
                materialized.materialized_range(),
                materialized.clamped_scroll,
                win.is_following_tail()
            );
        }
    } else {
        let text = buf.text();
        win.clamp_anchors_to_source(&text);
    }
    win.scroll_left = 0;
}

pub(super) fn prepare_transcript_window(
    transcript: &mut crate::app::transcript::TranscriptDocument,
    lua: &smelt_core::lua::runtime::LuaRuntime,
    theme: &std::sync::Arc<crate::smelt_edit::Theme>,
    ui: &mut crate::smelt_edit::Ui,
    request: crate::smelt_edit::MaterializeRequest,
    search_projection: TranscriptSearchProjection<'_>,
) {
    if crate::app::document::DocumentRegistry::resolve_optional(request.document_handle)
        != Some(crate::app::document::RegisteredDocument::Transcript)
    {
        return;
    }
    let viewport_rows = request.rect.height;
    let pending_restore;
    let fallback_cursor_screen_row = ui
        .win(request.win)
        .and_then(|win| win.cursor_screen_row(viewport_rows));
    let transcript_cursor_range;
    let suppress_cursor_screen_row_restore;
    let allow_cursor_search_range;
    let mut settled_search_match = None;
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
        let plan = match plan {
            Ok(plan) => plan,
            Err(error) => {
                record_transcript_projection_hydration_deferred(error);
                sync_retained_transcript_window(ui, request);
                return;
            }
        };
        pending_restore = transcript.take_pending_projection_restore();
        let Some(buf) = ui.buf_mut(request.buf) else {
            return;
        };
        let mut applied = transcript.project_applied_viewport(lua, buf, theme, plan);
        let search_anchor = (!request.follow_tail).then_some(search_anchor).flatten();
        let search_is_active = search_anchor.is_some();
        let mut search_projection_requested = false;
        let settled_search_range = if let Some(anchor) = search_anchor {
            let matched = transcript.resolve_search_range_anchor(lua, width, theme, anchor.clone());
            let mut resolved_match = matched;
            let search_needs_projection = width_changed
                || applied.cursor_range.is_some()
                || matched.range != anchor.fallback_range();
            if search_needs_projection {
                search_projection_requested = true;
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
                        resolved_match = transcript.resolve_search_range_anchor(
                            lua,
                            width,
                            theme,
                            anchor.clone(),
                        );
                        settled_search_match = Some(resolved_match);
                    }
                    Err(error) => record_transcript_projection_hydration_deferred(error),
                }
            }
            [
                applied.cursor_range,
                Some(resolved_match.range),
                Some(matched.range),
                Some(anchor.fallback_range()),
            ]
            .into_iter()
            .flatten()
            .find(|range| {
                transcript_search_range_matches_buffer(
                    buf,
                    applied.materialized_rows,
                    *range,
                    anchor.query(),
                )
            })
        } else {
            None
        };
        let desired_scroll_state = applied.scroll_state;
        suppress_cursor_screen_row_restore = settled_search_range.is_some();
        allow_cursor_search_range = search_is_active && !search_projection_requested;
        transcript_cursor_range = if search_is_active {
            settled_search_range
        } else {
            applied.cursor_range
        };
        let tdata = applied.materialized_rows;
        let backing_lines_tick = buf.lines_tick();
        debug_assert!(
            applied.has_exact_backing(viewport_rows),
            "interactive transcript projection escaped exact backing"
        );
        if let Some(win) = ui.win_mut(request.win) {
            debug_assert!(tdata.total_rows >= tdata.row_base);
            debug_assert!(
                tdata.clamped_scroll <= tdata.total_rows.saturating_sub(viewport_rows as _)
            );
            win.apply_materialized_rows_at_tick(tdata, backing_lines_tick);
            win.apply_projected_scroll(tdata.clamped_scroll, desired_scroll_state);
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
        } else if restore.cursor.is_none() && pending_restore.cursor_document_position.is_none() {
            restore.cursor = fallback_cursor_screen_row;
            restore.cursor_selection =
                crate::smelt_edit::CursorScreenRowSelection::SkipActiveSelection;
        }
        win.restore_document_view_screen_rows(buf, restore);
        if let Some(cursor) = pending_restore.cursor_document_position {
            let mut state = win.document_view_state();
            state.cursor = cursor;
            win.set_document_view_state(state);
        }
        let cursor_search_range = if allow_cursor_search_range {
            search_projection.anchor.as_ref().and_then(|anchor| {
                let cursor = win.row_cursor()?;
                let fallback = anchor.fallback_range();
                let range = crate::smelt_edit::DocRange {
                    start: crate::smelt_edit::DocPosition {
                        row: cursor.row,
                        byte_col: fallback.start.byte_col,
                    },
                    end: crate::smelt_edit::DocPosition {
                        row: cursor.row,
                        byte_col: fallback.end.byte_col,
                    },
                };
                transcript_search_range_matches_buffer(
                    buf,
                    win.materialized_rows()?,
                    range,
                    anchor.query(),
                )
                .then_some(range)
            })
        } else {
            None
        };
        if let Some(range) = transcript_cursor_range.or(cursor_search_range) {
            if win.set_row_cursor(buf, range.start) {
                if let Some(matched) = settled_search_match.filter(|matched| matched.range == range)
                {
                    *search_projection.match_after = Some(matched);
                } else {
                    *search_projection.range_after = Some(range);
                }
            }
        }
    }
    sync_retained_transcript_window(ui, request);
}

impl TuiApp {
    fn prepare_committed_transcript_view(
        &mut self,
        focused: bool,
    ) -> Option<crate::smelt_edit::PreparedWindowRequest> {
        self.complete_pending_transcript_details();
        let theme = self.ui.theme().clone();
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
            .filter(|(_, _)| {
                self.conversation
                    .transcript()
                    .has_pending_search_projection()
            })
            .map(|(matched, query)| {
                self.conversation
                    .transcript_search_range_anchor(matched, query)
            });
        let mut transcript_search_range_after_projection = None;
        let mut transcript_search_match_after_projection = None;
        let lua = self.lua.execution();
        let prepared_transcript = {
            let core = &mut self.core;
            let conversation = &mut self.conversation;
            let ui = &mut self.ui;
            smelt_core::host::scope_core(core, || {
                ui.prepare_split_window_with(crate::app::TRANSCRIPT_WIN, |ui, request| {
                    let projection_is_current = ui
                        .win(request.win)
                        .and_then(|win| {
                            ui.buf(request.buf).map(|buf| {
                                conversation.transcript().projected_view_is_current(
                                    &lua,
                                    buf,
                                    request.content_width.max(1),
                                    request.rect.height,
                                    request.scroll_top,
                                    win.materialized_rows(),
                                )
                            })
                        })
                        .unwrap_or(false);
                    if projection_is_current {
                        if !request.follow_tail {
                            conversation.prime_transcript_local_scroll_base(
                                &lua,
                                request.content_width.max(1),
                                request.rect.height,
                                request.scroll_top,
                            );
                        }
                        if let Some(anchor) = (!request.follow_tail)
                            .then_some(transcript_search_anchor.clone())
                            .flatten()
                        {
                            let matched = conversation.resolve_transcript_search_range_anchor(
                                &lua,
                                request.content_width.max(1),
                                &theme,
                                anchor.clone(),
                            );
                            let (win, buf) = ui.win_and_buf_mut(request.win, request.buf);
                            if let (Some(win), Some(buf)) = (win, buf) {
                                if let Some(materialized) = win.materialized_rows() {
                                    let range = matched.range;
                                    if transcript_search_range_matches_buffer(
                                        buf,
                                        materialized,
                                        range,
                                        anchor.query(),
                                    ) && win.set_row_cursor(buf, range.start)
                                    {
                                        transcript_search_match_after_projection = Some(matched);
                                    }
                                }
                            }
                        }
                        if !conversation.transcript_hydration_is_pending() {
                            conversation.trace_retained_transcript_frame(
                                &lua,
                                request.content_width.max(1),
                                request.rect.height,
                            );
                        }
                        sync_retained_transcript_window(ui, request);
                    } else {
                        conversation.prepare_transcript_window(
                            &lua,
                            &theme,
                            ui,
                            request,
                            TranscriptSearchProjection {
                                anchor: transcript_search_anchor.clone(),
                                range_after: &mut transcript_search_range_after_projection,
                                match_after: &mut transcript_search_match_after_projection,
                            },
                        );
                    }
                })
            })
        };
        if let Some(prepared) = prepared_transcript {
            let adjusted_scroll = self
                .ui
                .win_mut(prepared.win)
                .and_then(crate::smelt_edit::Window::sync_materialized_scroll_top);
            if let Some(scroll_top) = adjusted_scroll {
                self.conversation.reanchor_retained_transcript_viewport(
                    &lua,
                    prepared.content_width.max(1),
                    prepared.viewport_rect.height,
                    scroll_top,
                );
            }
        }
        self.sync_transcript_scrollbar_displays();
        let transcript_visible = prepared_transcript.is_some();
        if let Some(matched) = transcript_search_match_after_projection {
            self.overlays
                .replace_current_transcript_search_match(matched);
        } else if let Some(range) = transcript_search_range_after_projection {
            self.update_current_transcript_search_range(crate::app::TRANSCRIPT_WIN, range);
        }
        {
            let _perf = smelt_perf::perf::begin("compositor:capture_committed_transcript_view");
            self.capture_committed_transcript_view(
                focused && transcript_visible,
                transcript_visible,
            );
        }
        self.dispatch_pending_transcript_hydration();
        prepared_transcript
    }

    /// Paint each transcript-backed thumb where a pending scrollbar seek is
    /// headed while its records hydrate.
    fn sync_transcript_scrollbar_displays(&mut self) {
        let windows = [
            Some(crate::app::TRANSCRIPT_WIN),
            self.session_preview_window(),
        ];
        for window in windows.into_iter().flatten() {
            let pending = self
                .ui
                .win(window)
                .and_then(|window| window.viewport)
                .and_then(|viewport| viewport.scrollbar)
                .and_then(|bar| {
                    self.transcript_document_for_window(window)?
                        .pending_scrollbar_display_scroll_top(bar.total_rows, bar.viewport_rows)
                });
            if let Some(window) = self.ui.win_mut(window) {
                window.set_scrollbar_display_scroll_top(pending);
            }
        }
    }

    fn transcript_document_for_window(
        &self,
        window: crate::smelt_edit::WinId,
    ) -> Option<&crate::app::transcript::TranscriptDocument> {
        if window == crate::app::TRANSCRIPT_WIN {
            Some(self.conversation.transcript())
        } else {
            self.session_preview_document(window)
        }
    }

    fn dispatch_pending_transcript_hydration(&mut self) {
        let context_id = self.conversation.transcript_hydration_context_id();
        if let Some(worker) = self.transcript_hydration_worker.as_ref() {
            worker.set_context(context_id);
        }
        let Some(request) = self
            .conversation
            .take_pending_transcript_hydration_request()
        else {
            return;
        };
        let worker = self.transcript_hydration_worker.get_or_insert_with(|| {
            crate::app::transcript_hydration::TranscriptHydrationWorker::spawn(
                self.platform.app_event_sender(),
            )
        });
        worker.request(request);
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

    fn dispatch_committed_transcript_view(&mut self) -> bool {
        let Some(view) = self.conversation.committed_transcript_view() else {
            return false;
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
        let invoked = !prepared.is_empty();
        let lua = self.lua.execution();
        crate::lua::scope_app(self, move || {
            for (callback, payload) in prepared {
                if let Err(error) = callback.call::<()>(payload) {
                    lua.record_error(format!("transcript view watcher: {error}"));
                }
            }
        });
        invoked
    }

    pub(crate) fn render_normal(&mut self) {
        let mut stdout = std::io::stdout();
        self.render_normal_to(&mut stdout);
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
        self.render_scheduled_to(out);
        true
    }

    pub(crate) fn render_pending_transient_frame_to<W: std::io::Write>(
        &mut self,
        out: &mut W,
    ) -> bool {
        if !self.frame_scheduler.has_pending() {
            return false;
        }
        self.render_scheduled_to(out);
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
        let arrived_at = self.core.clock.instant_now();
        if self.render_transient_frame_before_engine_event_to(&ev, out) {
            on_transient_frame(self);
        }
        let continuation_started = match &ev {
            protocol::EngineEvent::EngineAskDelta { id, .. } => {
                self.continuing_engine_ask_ids.contains(id)
            }
            _ => self.conversation.has_live_transcript_blocks(),
        };
        if is_engine_continuation(&ev) && continuation_started {
            self.queue_engine_continuation_at(ev, arrived_at);
            self.frame_scheduler.request_stream();
            return true;
        }
        let keep_streaming = self.dispatch_engine_event_at(ev, arrived_at);
        // Engine output is machine-paced: coalesce it with streaming frames
        // instead of painting once per event.
        self.request_continuation_render();
        keep_streaming
    }

    /// Paint streamed output that has not reached the screen before an event
    /// that may finish or replace it, so every streamed state is shown.
    pub(crate) fn render_transient_frame_before_engine_event_to<W: std::io::Write>(
        &mut self,
        ev: &protocol::EngineEvent,
        out: &mut W,
    ) -> bool {
        if is_engine_continuation(ev) || !self.frame_scheduler.has_pending_stream() {
            return false;
        }
        self.render_pending_transient_frame_to(out)
    }

    pub(crate) fn refresh_main_layout(&mut self) -> (layout::Rect, u16) {
        self.lua.shared().request_layout_refresh();
        self.ensure_main_layout()
    }

    /// Measure the same retained prompt projection and wrapping used for paint.
    fn measure_prompt_input_rows(&mut self, width: u16) -> u16 {
        let placeholder_is_empty = self
            .prompt
            .placeholder_text(crate::app::PROMPT_WIN)
            .is_none_or(str::is_empty);
        let ctx = crate::input::prompt_ctx_mut(&mut self.ui);
        let usable = ctx.win.config.gutters.content_width(width);
        ctx.buf.ensure_rendered_at(usable);
        ctx.win.wrap_cursor_padding = !ctx.buf.source().is_empty() || placeholder_is_empty;
        ctx.win.ensure_layout(ctx.buf, usable);
        ctx.win.layout().visual_count().clamp(1, u16::MAX as usize) as u16
    }

    fn ensure_main_layout(&mut self) -> (layout::Rect, u16) {
        if smelt_core::host::host_access_active() {
            self.lua.shared().request_layout_refresh();
            return (self.layout.prompt, self.layout.viewport_rows());
        }
        let applying_deferred_layout = self.lua.shared().take_layout_refresh();
        let (term_w, term_h) = self.ui.terminal_size();
        let wrapped_rows = self.measure_prompt_input_rows(term_w);
        // Auto-height keeps the transcript usable; a deliberate manual resize
        // can claim more room for prompt review without taking the full screen.
        let input_rows = self.prompt.resolve_height(wrapped_rows, term_h);
        let dialog = self.active_docked_dialog();
        let dialog_buffers = dialog
            .and_then(|id| self.ui.docked_surface_buffer_revisions(id))
            .unwrap_or_default();
        let layout_inputs = crate::app::MainLayoutInputs {
            terminal_width: term_w,
            terminal_height: term_h,
            prompt_input_rows: input_rows,
            prompt_has_stash: self.prompt.has_stash(),
            prompt_queue_revision: self.prompt.queue_revision(),
            dialog,
            dialog_buffers,
        };
        if !applying_deferred_layout && self.main_layout_inputs.as_ref() == Some(&layout_inputs) {
            return (self.layout.prompt, self.layout.viewport_rows());
        }
        let tree = self
            .invoke_lua_layout_composer(term_w, term_h, input_rows)
            .unwrap_or_else(|| self.fallback_main_layout(term_w, term_h, input_rows));
        let previous_transcript_rect = self.layout.transcript;
        let transcript_was_pinned_to_tail = self
            .window_scroll_snapshot(crate::app::TRANSCRIPT_WIN)
            .is_some_and(|scroll| {
                self.ui
                    .win(crate::app::TRANSCRIPT_WIN)
                    .is_some_and(|win| scroll.is_pinned_to_tail(win))
            });
        self.ui.set_layout(tree);
        // A toast restored inside a Lua callback can precede the deferred
        // prompt layout. Resolve its anchor again against the mounted chrome.
        if let Some(id) = self
            .overlays
            .notification()
            .and_then(|notification| self.ui.overlay_for_leaf(notification.win))
        {
            let anchor = layout::anchor_for_prompt_toast(&self.ui);
            if let Some(overlay) = self.ui.overlay_mut(id) {
                overlay.anchor = anchor;
            }
        }
        let next_layout = layout::LayoutState::from_ui(&self.ui);
        let transcript_viewport_resized = next_layout.transcript.width
            != previous_transcript_rect.width
            || next_layout.transcript.height != previous_transcript_rect.height;
        if transcript_was_pinned_to_tail && transcript_viewport_resized {
            self.transcript_win_mut().follow_tail();
        }
        self.layout = next_layout;
        self.main_layout_inputs = Some(layout_inputs);
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
    ///
    /// A frame composes the UI into a grid, then presents it: present-time
    /// effects (the stream reveal, then frame paints) draw over a copy and the
    /// copy is flushed. Rendering on demand always composes, since the caller
    /// may have changed anything.
    pub(crate) fn render_normal_to<W: std::io::Write>(&mut self, out: &mut W) {
        self.render_frame_to(out, true);
    }

    /// Render the frame the scheduler asked for. When only present-time
    /// effects asked, the last composed grid is presented again.
    pub(crate) fn render_scheduled_to<W: std::io::Write>(&mut self, out: &mut W) {
        self.render_frame_to(out, false);
    }

    pub(crate) fn render_scheduled(&mut self) {
        self.render_scheduled_to(&mut std::io::stdout());
    }

    fn render_frame_to<W: std::io::Write>(&mut self, out: &mut W, compose: bool) {
        self.drive_woken_lua_tasks();
        smelt_view::display_layout::update_kitty_geometry();
        let frame_trace = self
            .frame_scheduler
            .begin_frame(self.core.clock.instant_now());
        let _perf = smelt_perf::perf::begin("app:tick_compositor");
        let size = self.ui.terminal_size();
        let composed_stale = self
            .composed
            .as_ref()
            .is_none_or(|composed| (composed.grid().width(), composed.grid().height()) != size);
        let compose = compose || frame_trace.is_none_or(|trace| trace.compose) || composed_stale;
        let clock = if compose {
            Some(self.compose_frame())
        } else {
            None
        };
        self.present_frame(out, clock);
        if let Some(trace) = frame_trace {
            FrameScheduler::record_flushed(trace);
        }
    }

    /// Bring the UI up to date and paint it into [`TuiApp::composed`].
    /// Returns the paint clock its leaf paints saw.
    fn compose_frame(&mut self) -> crate::lua::paint::PaintClock {
        let _perf = smelt_perf::perf::begin("compositor:compose");
        self.apply_pending_transcript_work();
        // Published after queued work is applied: it can end the turn, and
        // renderers must paint that state rather than the previous one.
        self.publish_diff_signals();
        self.plugin_streams
            .retain(|stream| stream.strong_count() > 0);
        self.refresh_pending_tool_draft_summaries();
        if !self.transcript_work.is_empty() && !self.transcript_work.front_waits_for_hydration() {
            self.frame_scheduler.request_stream();
        }
        // Signal subscribers drive retained window and layout invalidation. Drain
        // them before querying dirty state so each frame observes one coherent
        // semantic update rather than repainting a frame late.
        self.drain_signals_pending();

        self.ui.resolve_tail_scrolls();
        self.ui.sync_scroll_links();

        let (has_prompt_cursor, has_transcript_cursor) = self.compute_cursor_ownership();

        // Hidden is the right baseline; sync paths below set Block when focus owns the caret.
        self.ui
            .set_cursor_shape(crate::smelt_edit::CursorShape::Hidden);

        // Resolve widths before retained renderers run, then measure their
        // updated content before preparing the transcript or painting a frame.
        let (prompt_rect, _) = {
            let _p = smelt_perf::perf::begin("compositor:layout");
            self.ensure_main_layout()
        };

        // Freeze timer/spinner while a blocking dialog is up. Done before
        // Lua renderers run so the prompt top-bar indicator they paint
        // this frame already reflects the pause.
        self.set_agent_blocked_paused(self.ui.active_modal_blocks_agent());
        let now = self.core.clock.instant_now();
        self.conversation.sync_active_tool_elapsed(now);
        self.sync_transcript_renderer_generation();

        let prepared_transcript =
            self.reconcile_main_layout(prompt_rect, has_prompt_cursor, has_transcript_cursor);
        let notified = {
            let _perf = smelt_perf::perf::begin("compositor:dispatch_committed_transcript_view");
            self.dispatch_committed_transcript_view()
        };
        if notified {
            // Observers can populate overlay renderers for this frame. Root
            // geometry changes apply next frame, not to the view just delivered.
            self.drain_signals_pending();
            let dirty = self
                .lua
                .shared()
                .win_renderers
                .lock()
                .is_ok_and(|renderers| renderers.values().any(|renderer| renderer.dirty));
            if dirty {
                self.dispatch_lua_renderers(None);
            }
            if self.lua.shared().layout_refresh_pending() {
                self.request_continuation_render();
            }
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

        // Transcript content was materialized by the committed-view phase. The
        // prepare pass still materializes other row-backed windows and applies search.
        let prepared_frame = {
            let _p = smelt_perf::perf::begin("compositor:prepare");
            let overlays = &mut self.overlays;
            let search_session = overlays
                .search_session()
                .map(crate::app::search::SearchRenderSession::from);
            self.ui.prepare_frame(
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
                    win.clear_search_ranges();
                    if let Some(search) =
                        search_session.as_ref().filter(|s| s.target == request.win)
                    {
                        search.apply_to_window(win, buf, request.viewport_rect.height);
                    }
                },
            )
        };

        // Lua paint callbacks need a scoped app host, which cannot alias the UI
        // borrow. The frame owns its grid and borrows the UI only while painting
        // built-in commands, so each Lua leaf paints directly into the frame in
        // its layer order.
        let clock = self.paint_timeline.advance(
            self.core.monotonic_ms(),
            self.platform.terminal_is_focused(),
        );
        let mut frame = self.ui.begin_frame(prepared_frame);
        while let Some(leaf) = frame.paint_to_next_leaf(&self.ui) {
            let Some(slot) = self.paint_registry.leaf(leaf.id) else {
                continue;
            };
            self.run_paint(
                slot,
                &mut frame.grid_mut().slice_mut(leaf.rect),
                &leaf.context,
                &clock,
                None,
            );
        }
        let composed = frame.finish();
        self.measure_stream_reveals(&composed);
        if self
            .paint_registry
            .frame_paints()
            .iter()
            .any(|slot| slot.options.history())
        {
            let areas = self.scroll_areas(composed.windows());
            self.paint_timeline.compose_history(areas);
        } else {
            self.paint_timeline.drop_history();
        }
        if let Some(previous) = self.composed.replace(composed) {
            self.ui.recycle_frame(previous);
        }
        clock
    }

    /// Count what each stream reveal gained on the frame just composed.
    fn measure_stream_reveals(&mut self, composed: &crate::smelt_edit::ComposedFrame) {
        let now = self.core.clock.instant_now();
        let painted = |shows: &dyn Fn(&crate::smelt_edit::PaintedWindow) -> bool| {
            composed
                .windows()
                .iter()
                .find(|painted| shows(painted))
                .copied()
        };
        if self.conversation.reveal_mut().is_tracking() {
            let layout: std::collections::HashMap<u64, _> = self
                .conversation
                .transcript()
                .visible_block_layout()
                .map(|(block, start, rows)| (block.get(), (start, rows)))
                .collect();
            let window = painted(&|painted| painted.win == crate::app::TRANSCRIPT_WIN);
            self.conversation.reveal_mut().measure(
                composed,
                window,
                |id| layout.get(&id).copied(),
                now,
            );
        }
        for stream in self
            .plugin_streams
            .iter()
            .filter_map(std::rc::Weak::upgrade)
        {
            let mut stream = stream.borrow_mut();
            if !stream.reveal_mut().is_tracking() {
                continue;
            }
            // The stream's buffer is its one segment.
            let buf = stream.buf();
            let window = painted(&|painted| painted.buf == buf);
            let rows = self
                .ui
                .buf(buf)
                .map_or(0, |buf| buf.line_count() as crate::smelt_edit::RowIndex);
            stream
                .reveal_mut()
                .measure(composed, window, |_| Some((0, rows)), now);
        }
    }

    /// Present the composed frame: hide unrevealed stream glyphs, run frame
    /// paints over a copy and flush it. `clock` is the composing frame's when
    /// this frame composed, so leaf and frame paints share one frame.
    fn present_frame<W: std::io::Write>(
        &mut self,
        out: &mut W,
        clock: Option<crate::lua::paint::PaintClock>,
    ) {
        let Some(composed) = self.composed.take() else {
            return;
        };
        let _perf = smelt_perf::perf::begin("compositor:present");
        let now = self.core.clock.instant_now();
        let mut frame = self.ui.begin_present(&composed);
        let grid = frame.grid_mut();
        let mut revealing = self.conversation.reveal_mut().present(grid, &composed, now);
        for stream in self
            .plugin_streams
            .iter()
            .filter_map(std::rc::Weak::upgrade)
        {
            revealing |= stream
                .borrow_mut()
                .reveal_mut()
                .present(grid, &composed, now);
        }
        if revealing {
            self.frame_scheduler
                .request_present(FrameUrgency::Continuation);
        }
        let clock = clock.unwrap_or_else(|| {
            self.paint_timeline.advance(
                self.core.monotonic_ms(),
                self.platform.terminal_is_focused(),
            )
        });
        let frame_paints = self.paint_registry.frame_paints();
        let changes = frame_paints
            .iter()
            .any(|slot| slot.options.history())
            .then(|| self.paint_timeline.present_history(grid));
        crate::lua::paint::scope_composed(&composed, || {
            self.run_frame_paints(&frame_paints, grid, &clock, changes.as_ref());
        });
        let _ = self.ui.flush_present(out, frame, &composed);
        self.composed = Some(composed);
    }

    /// Where each painted window's content sat, so frame paints can tell
    /// scrolling from change.
    fn scroll_areas(
        &self,
        windows: &[crate::smelt_edit::PaintedWindow],
    ) -> Vec<crate::lua::paint::ScrollArea> {
        windows
            .iter()
            .map(|painted| {
                let origin = |row: crate::smelt_edit::RowIndex| {
                    i64::from(painted.text.y) + row as i64 - painted.scroll_top as i64
                };
                let anchors = if painted.win == crate::app::well_known::TRANSCRIPT_WIN {
                    self.conversation
                        .transcript()
                        .visible_block_layout()
                        .map(|(block, first_row, _)| (block.get(), origin(first_row)))
                        .collect()
                } else {
                    vec![(0, origin(0))]
                };
                crate::lua::paint::ScrollArea {
                    win: painted.win,
                    area: painted.text,
                    anchors,
                }
            })
            .collect()
    }

    /// Run frame-stage paints over the finished screen, in order.
    fn run_frame_paints(
        &mut self,
        paints: &[crate::lua::paint::PaintSlot],
        frame: &mut crate::smelt_edit::Grid,
        clock: &crate::lua::paint::PaintClock,
        changes: Option<&crate::lua::paint::FrameChanges>,
    ) {
        if paints.is_empty() {
            return;
        }
        let clock = crate::lua::paint::PaintClock {
            cursor: frame.terminal_cursor_position(),
            ..*clock
        };
        let area = crate::smelt_edit::Rect::new(0, 0, frame.width(), frame.height());
        let context = crate::smelt_edit::DrawContext {
            terminal_width: frame.width(),
            terminal_height: frame.height(),
            theme: std::sync::Arc::clone(self.ui.theme()),
            ..Default::default()
        };
        for slot in paints {
            self.run_paint(*slot, &mut frame.slice_mut(area), &context, &clock, changes);
        }
    }

    fn run_paint(
        &mut self,
        slot: crate::lua::paint::PaintSlot,
        slice: &mut crate::smelt_edit::GridSlice<'_>,
        context: &crate::smelt_edit::DrawContext,
        clock: &crate::lua::paint::PaintClock,
        changes: Option<&crate::lua::paint::FrameChanges>,
    ) {
        let lua = self.lua.execution();
        let result = crate::lua::scope_app(self, || {
            crate::lua::paint::paint(&lua, slot, slice, context, clock, changes)
        });
        if let Err(error) = result {
            self.fail_paint(slot.id, &error);
        }
    }

    /// Remove a paint whose callback raised `error`. It would raise it again
    /// on every frame, so it is reported once; `/reload` restores it.
    pub(crate) fn fail_paint(
        &mut self,
        paint: crate::smelt_edit::layout::PaintId,
        error: &mlua::Error,
    ) {
        if self.remove_paint(paint) {
            self.record_notice(
                smelt_core::messages::MessageKind::Error,
                "lua".into(),
                format!("smelt.paint removed after error: {error}"),
            );
        }
    }

    pub(crate) fn remove_paint(&mut self, paint: crate::smelt_edit::layout::PaintId) -> bool {
        self.request_urgent_render();
        for callback_id in self.ui.leaf_clear_callbacks(paint) {
            self.lua.remove_callback(callback_id);
        }
        let Some(slot) = self.paint_registry.unregister(paint) else {
            return false;
        };
        for callback_id in slot.callbacks() {
            self.lua.remove_callback(callback_id);
        }
        true
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
            // A docked dialog takes the prompt out of the layout. The layout above
            // was built at zero width, so drop the viewport from its last frame.
            if prompt_rect.width == 0 {
                pctx.win.viewport = None;
            }
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
        let layout = crate::lua::ui_ops::build_layout_tree(self, &node, &mut window_leaves)
            .and_then(|(_, tree)| {
                crate::lua::ui_ops::validate_split_mounts(
                    &tree,
                    self.ui.layout_trees().skip(1),
                    |_| false,
                )?;
                Ok(tree)
            });
        match layout {
            Ok(tree) => Some(tree),
            Err(e) => {
                self.record_lua_error(format!("smelt.ui.layout composer tree: {e}"));
                None
            }
        }
    }

    /// Resolve renderer-driven sizing before notifying committed-view observers.
    fn reconcile_main_layout(
        &mut self,
        mut prompt_rect: layout::Rect,
        has_prompt_cursor: bool,
        has_transcript_cursor: bool,
    ) -> Option<crate::smelt_edit::PreparedWindowRequest> {
        const MAX_LAYOUT_PASSES: usize = 8;
        let dialog_windows = self.active_docked_dialog().and_then(|id| {
            let modal = self.ui.docked_surface(id)?.modal();
            self.ui.modal_leaves(modal).map(<[_]>::to_vec)
        });
        if let Some(windows) = dialog_windows {
            let _perf = smelt_perf::perf::begin("compositor:lua_renderers");
            self.dispatch_lua_renderers(Some(&windows));
            (prompt_rect, _) = self.ensure_main_layout();
        }
        let mut passes = 0;
        loop {
            passes += 1;
            {
                let _perf = smelt_perf::perf::begin("compositor:input");
                self.sync_input_layer(prompt_rect, has_prompt_cursor);
            }
            let prepared = {
                let _perf = smelt_perf::perf::begin("compositor:prepare_committed_transcript_view");
                self.prepare_committed_transcript_view(has_transcript_cursor)
            };
            self.publish_view_signals();
            self.drain_signals_pending();
            {
                let _perf = smelt_perf::perf::begin("compositor:lua_renderers");
                self.dispatch_lua_renderers(None);
            }
            if !self.lua.shared().layout_refresh_pending() {
                return prepared;
            }
            if passes == MAX_LAYOUT_PASSES {
                self.lua.shared().take_layout_refresh();
                self.record_lua_error(format!("UI layout did not settle after {MAX_LAYOUT_PASSES} passes; check layout composer and window renderers"));
                return prepared;
            }
            (prompt_rect, _) = self.ensure_main_layout();
        }
    }

    /// Invoke dirty Lua renderers registered via `Win:set_renderer(fn)`.
    /// Each callback owns retained backing-buffer content and runs after
    /// registration, invalidation, or a change in resolved content size. Unmounted
    /// windows stay dirty; destroyed windows release their renderer callbacks.
    /// Dispatching every window answers the pending repaint request.
    fn dispatch_lua_renderers(&mut self, windows: Option<&[crate::smelt_edit::WinId]>) {
        let lua = self.lua.lua();
        let shared = self.lua.shared();
        if windows.is_none() {
            shared.take_win_repaint();
        }
        // Snapshot dirty callbacks so the registry mutex is not held across Lua
        // calls. Mark them clean first so a callback can invalidate itself for a
        // later frame without that request being overwritten.
        let entries: Vec<(crate::smelt_edit::WinId, mlua::Function)> = {
            let mut guard = match shared.win_renderers.lock() {
                Ok(guard) => guard,
                Err(_) => return,
            };
            if guard.is_empty() {
                return;
            }
            let sizes = self.ui.resolved_win_sizes();
            let mut entries = Vec::new();
            guard.retain(|raw_id, renderer| {
                let win_id = crate::smelt_edit::WinId(*raw_id);
                let Some(&size) = sizes.get(&win_id) else {
                    if self.ui.win(win_id).is_none() {
                        return false;
                    }
                    renderer.dirty = true;
                    renderer.rendered_size = None;
                    return true;
                };
                if windows.is_some_and(|windows| !windows.contains(&win_id))
                    || (!renderer.dirty && renderer.rendered_size == Some(size))
                {
                    return true;
                }
                renderer.dirty = false;
                renderer.rendered_size = Some(size);
                if let Ok(function) = lua.registry_value::<mlua::Function>(&renderer.handle.key) {
                    entries.push((win_id, function));
                }
                true
            });
            entries
        };
        for (win_id, function) in entries {
            if self.ui.win(win_id).is_none() {
                continue;
            }
            let win_ud = crate::lua::api::win::LuaWin { id: win_id };
            let result = crate::lua::scope_app(self, move || function.call::<()>((win_ud,)));
            if let Err(error) = result {
                self.record_lua_error(format!("win renderer for {win_id:?}: {error}"));
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
    fn unchanged_large_prompt_height_reuses_projection_and_wrapping_without_allocating() {
        use crate::app::test_harness::{SourceEvent, TestApp};

        for input in ["x".repeat(65_537), "界".repeat(32_768)] {
            let mut app = TestApp::builder().with_vim(false).build();
            app.set_terminal_size(100, 32);
            app.feed_one(SourceEvent::Term(crossterm::event::Event::Paste(input)));
            app.render_silent();
            let expected = app.tui.measure_prompt_input_rows(100);
            assert!(expected > 600);
            let enabled = smelt_perf::alloc::enabled();
            smelt_perf::alloc::set_enabled(true);
            let before = smelt_perf::alloc::thread_snapshot();
            for _ in 0..10 {
                assert_eq!(app.tui.measure_prompt_input_rows(100), expected);
            }
            let after = smelt_perf::alloc::thread_snapshot();
            smelt_perf::alloc::set_enabled(enabled);
            assert_eq!(after, before, "unchanged height measurement allocated");
        }
    }

    #[test]
    fn prompt_height_and_paint_share_cursor_padding_and_gutter_width() {
        use crate::app::test_harness::{SourceEvent, TestApp};

        let mut app = TestApp::builder().with_vim(false).build();
        let width = 80;
        let usable = app
            .tui
            .ui
            .win(crate::app::PROMPT_WIN)
            .unwrap()
            .config
            .gutters
            .content_width(width);
        let text = "x".repeat(usable as usize);
        app.tui
            .set_placeholder(crate::app::PROMPT_WIN, text.clone());
        assert_eq!(app.tui.measure_prompt_input_rows(width), 1);
        assert!(
            !app.tui
                .ui
                .win(crate::app::PROMPT_WIN)
                .unwrap()
                .wrap_cursor_padding
        );
        app.feed_one(SourceEvent::Term(crossterm::event::Event::Paste(text)));
        assert_eq!(app.tui.measure_prompt_input_rows(width), 2);
        assert!(
            app.tui
                .ui
                .win(crate::app::PROMPT_WIN)
                .unwrap()
                .wrap_cursor_padding
        );
        app.press(crossterm::event::KeyCode::Backspace);
        assert_eq!(app.tui.measure_prompt_input_rows(width), 1);
        app.tui
            .ui
            .win_mut(crate::app::PROMPT_WIN)
            .unwrap()
            .config
            .gutters
            .pad_left += 2;
        assert_eq!(app.tui.measure_prompt_input_rows(width), 2);
        assert_eq!(
            app.tui
                .ui
                .win(crate::app::PROMPT_WIN)
                .unwrap()
                .layout()
                .visual_count(),
            2
        );
    }

    #[test]
    fn prompt_height_saturates_instead_of_wrapping_at_terminal_row_limit() {
        use crate::app::test_harness::{SourceEvent, TestApp};

        let mut app = TestApp::builder().with_vim(false).build();
        app.feed_one(SourceEvent::Term(crossterm::event::Event::Paste(
            "\n".repeat(u16::MAX as usize),
        )));
        assert_eq!(app.tui.measure_prompt_input_rows(80), u16::MAX);
    }

    #[test]
    fn continuation_requests_wait_for_frame_interval() {
        let start = std::time::Instant::now();
        let mut scheduler = FrameScheduler::default();
        scheduler.begin_frame(start);
        scheduler.request(FrameUrgency::Continuation);

        assert!(!scheduler.is_due(start + std::time::Duration::from_millis(15)));
        assert_eq!(
            scheduler.next_delay(start + std::time::Duration::from_millis(15)),
            Some(std::time::Duration::from_millis(1))
        );
        assert!(scheduler.is_due(start + CONTINUATION_FRAME_INTERVAL));
    }

    #[test]
    fn urgent_request_promotes_pending_continuation_frame() {
        let start = std::time::Instant::now();
        let mut scheduler = FrameScheduler::default();
        scheduler.begin_frame(start);
        scheduler.request(FrameUrgency::Continuation);
        scheduler.request(FrameUrgency::Urgent);

        assert!(scheduler.is_due(start + std::time::Duration::from_millis(2)));
        let trace = scheduler
            .begin_frame(start + std::time::Duration::from_millis(2))
            .expect("promoted frame");
        assert_eq!(trace.id, 1);
        assert_eq!(trace.urgency, FrameUrgency::Urgent);
    }

    #[test]
    fn animation_and_continuation_share_earliest_deadline() {
        let start = std::time::Instant::now();
        let mut scheduler = FrameScheduler::default();
        scheduler.begin_frame(start);
        scheduler.request(FrameUrgency::Animation(
            start + std::time::Duration::from_millis(40),
        ));
        scheduler.request(FrameUrgency::Continuation);

        assert!(!scheduler.is_due(start + std::time::Duration::from_millis(15)));
        assert!(scheduler.is_due(start + CONTINUATION_FRAME_INTERVAL));
    }

    #[test]
    fn animation_deadlines_wait_for_the_frame_grid() {
        let start = std::time::Instant::now();
        let mut scheduler = FrameScheduler::default();
        scheduler.begin_frame(start);
        scheduler.request_present(FrameUrgency::Animation(
            start + std::time::Duration::from_millis(1),
        ));

        assert!(!scheduler.is_due(start + std::time::Duration::from_millis(15)));
        assert!(scheduler.is_due(start + CONTINUATION_FRAME_INTERVAL));
    }

    #[test]
    fn animation_deadlines_count_from_the_request_not_the_last_frame() {
        let start = std::time::Instant::now();
        let mut scheduler = FrameScheduler::default();
        scheduler.begin_frame(start);
        let requested = start + std::time::Duration::from_millis(500);
        scheduler.request_present(FrameUrgency::Animation(
            requested + std::time::Duration::from_millis(100),
        ));

        assert!(!scheduler.is_due(requested));
        assert_eq!(
            scheduler.next_delay(requested),
            Some(std::time::Duration::from_millis(100))
        );
        assert!(scheduler.is_due(requested + std::time::Duration::from_millis(100)));
    }

    #[test]
    fn repeated_continuation_requests_coalesce_into_one_trace() {
        let start = std::time::Instant::now();
        let mut scheduler = FrameScheduler::default();
        scheduler.request(FrameUrgency::Continuation);
        for _ in 1..100 {
            scheduler.request(FrameUrgency::Continuation);
        }

        let trace = scheduler.begin_frame(start).expect("coalesced frame");
        assert_eq!(trace.id, 1);
        assert!(!scheduler.has_pending());
    }

    #[test]
    fn continuation_mutations_apply_together_at_the_frame_boundary() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.start_turn(1);
        let mut sink = std::io::sink();

        app.tui.dispatch_engine_event_in_render_loop_to(
            protocol::EngineEvent::TextDelta {
                delta: "first".into(),
            },
            &mut sink,
            |_| {},
        );
        app.tui.render_pending_transient_frame_to(&mut sink);

        app.tui.dispatch_engine_event_in_render_loop_to(
            protocol::EngineEvent::TextDelta {
                delta: " second".into(),
            },
            &mut sink,
            |_| {},
        );
        app.tui.dispatch_engine_event_in_render_loop_to(
            protocol::EngineEvent::TextDelta {
                delta: " third".into(),
            },
            &mut sink,
            |_| {},
        );

        fn streamed_text(app: &crate::app::test_harness::TestApp) -> String {
            let id = app
                .tui
                .conversation
                .transcript()
                .history()
                .last_block_id()
                .expect("streamed text block");
            let smelt_transcript::transcript_model::Block::Text { content } = app
                .tui
                .conversation
                .transcript()
                .history()
                .block(id)
                .expect("streamed text")
            else {
                panic!("last block is streamed text");
            };
            content.snapshot()
        }
        assert_eq!(streamed_text(&app), "first");

        app.clock.advance(6 * CONTINUATION_FRAME_INTERVAL);
        assert!(app.tui.render_pending_transient_frame_to(&mut sink));
        assert_eq!(streamed_text(&app), "first second third");
        let screen = app.tui.ui.flushed_snapshot().text();
        assert!(
            screen.contains('f') && !screen.contains("first second third"),
            "the new text types in over later frames: {screen}"
        );
        app.clock.advance(crate::app::stream_reveal::MAX_SPREAD);
        assert!(app.tui.render_pending_transient_frame_to(&mut sink));
        let screen = app.tui.ui.flushed_snapshot().text();
        assert!(screen.contains("first second third"), "{screen}");
    }

    #[test]
    fn response_draft_rejection_waits_for_queued_stream_continuations() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.start_turn(1);
        app.dispatch_engine_event(protocol::EngineEvent::TextDelta {
            delta: "rejected first".into(),
        });
        app.tui
            .queue_engine_continuation(protocol::EngineEvent::TextDelta {
                delta: " rejected queued".into(),
            });
        app.dispatch_engine_event(protocol::EngineEvent::ResponseDraftRejected);
        app.tui
            .queue_engine_continuation(protocol::EngineEvent::TextDelta {
                delta: "valid answer".into(),
            });
        app.tui.apply_pending_transcript_work();
        app.render_to_frame();
        app.clock.advance(crate::app::stream_reveal::MAX_SPREAD);
        let frame = app.render_to_frame().text();
        assert!(frame.contains("valid answer"));
        assert!(!frame.contains("rejected"));
        app.assert_invariants();
    }

    #[test]
    fn response_draft_acceptance_waits_for_queued_stream_continuations() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.start_turn(1);
        app.dispatch_engine_event(protocol::EngineEvent::TextDelta {
            delta: "accepted first".into(),
        });
        app.tui
            .queue_engine_continuation(protocol::EngineEvent::TextDelta {
                delta: " accepted queued".into(),
            });
        app.dispatch_engine_event(protocol::EngineEvent::ResponseDraftAccepted);
        app.tui
            .queue_engine_continuation(protocol::EngineEvent::TextDelta {
                delta: "rejected next".into(),
            });
        app.dispatch_engine_event(protocol::EngineEvent::ResponseDraftRejected);
        app.tui.apply_pending_transcript_work();
        let frame = app.render_to_frame().text();
        assert!(frame.contains("accepted first accepted queued"));
        assert!(!frame.contains("rejected"));
        app.assert_invariants();
    }

    #[test]
    fn every_engine_continuation_uses_the_shared_classification() {
        let events = [
            protocol::EngineEvent::ReasoningPartDelta {
                id: "reasoning".into(),
                kind: protocol::ReasoningKind::Raw,
                delta: "delta".into(),
                title: None,
            },
            protocol::EngineEvent::TextDelta {
                delta: "delta".into(),
            },
            protocol::EngineEvent::ToolCallDraftDelta {
                stream_id: "draft".into(),
                call_id: None,
                tool_name: Some("read_file".into()),
                delta: "delta".into(),
            },
            protocol::EngineEvent::ToolOutput {
                invocation_id: protocol::InvocationId::new(1),
                call_id: "call".into(),
                line: "delta".into(),
            },
            protocol::EngineEvent::EngineAskDelta {
                id: 1,
                delta: "delta".into(),
            },
        ];

        assert!(events.iter().all(is_engine_continuation));
        assert!(!is_engine_continuation(
            &protocol::EngineEvent::ReasoningPartStarted {
                id: "reasoning".into(),
                kind: protocol::ReasoningKind::Raw,
            }
        ));
    }

    #[test]
    fn engine_ask_keeps_only_its_first_delta_urgent() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let mut sink = std::io::sink();

        app.tui.dispatch_engine_event_in_render_loop_to(
            protocol::EngineEvent::EngineAskDelta {
                id: 7,
                delta: "first".into(),
            },
            &mut sink,
            |_| {},
        );
        assert!(app.tui.continuing_engine_ask_ids.contains(&7));
        assert!(app.tui.transcript_work.is_empty());
        assert!(app.tui.render_pending_transient_frame_to(&mut sink));

        for delta in [" second", " third"] {
            app.tui.dispatch_engine_event_in_render_loop_to(
                protocol::EngineEvent::EngineAskDelta {
                    id: 7,
                    delta: delta.into(),
                },
                &mut sink,
                |_| {},
            );
        }
        assert_eq!(app.tui.transcript_work.len(), 2);

        app.tui.dispatch_engine_event_in_render_loop_to(
            protocol::EngineEvent::EngineAskResponse {
                id: 7,
                message: None,
                error: None,
            },
            &mut sink,
            |_| {},
        );
        assert!(app.tui.transcript_work.is_empty());
        assert!(!app.tui.continuing_engine_ask_ids.contains(&7));
    }

    #[test]
    fn cancellation_discards_queued_text_reasoning_and_draft_continuations() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.start_turn(1);
        let mut sink = std::io::sink();

        app.tui.dispatch_engine_event_in_render_loop_to(
            protocol::EngineEvent::TextDelta {
                delta: "visible before cancellation".into(),
            },
            &mut sink,
            |_| {},
        );
        app.tui.render_pending_transient_frame_to(&mut sink);
        app.tui.dispatch_engine_event_in_render_loop_to(
            protocol::EngineEvent::ReasoningPartStarted {
                id: "reasoning".into(),
                kind: protocol::ReasoningKind::Raw,
            },
            &mut sink,
            |_| {},
        );
        app.tui.dispatch_engine_event_in_render_loop_to(
            protocol::EngineEvent::ToolCallDraftStarted {
                stream_id: "draft".into(),
                call_id: None,
                tool_name: Some("read_file".into()),
                native: false,
            },
            &mut sink,
            |_| {},
        );
        app.tui.dispatch_engine_event_in_render_loop_to(
            protocol::EngineEvent::TextDelta {
                delta: " queued text".into(),
            },
            &mut sink,
            |_| {},
        );
        app.tui.dispatch_engine_event_in_render_loop_to(
            protocol::EngineEvent::ReasoningPartDelta {
                id: "reasoning".into(),
                kind: protocol::ReasoningKind::Raw,
                delta: "queued reasoning".into(),
                title: None,
            },
            &mut sink,
            |_| {},
        );
        app.tui.dispatch_engine_event_in_render_loop_to(
            protocol::EngineEvent::ToolCallDraftDelta {
                stream_id: "draft".into(),
                call_id: None,
                tool_name: Some("read_file".into()),
                delta: "queued draft".into(),
            },
            &mut sink,
            |_| {},
        );
        assert_eq!(app.tui.transcript_work.len(), 3);

        app.tui.cancel_agent();
        assert!(app.tui.transcript_work.is_empty());
        let frame = app.render_to_frame();
        assert!(frame.text().contains("visible before cancellation"));
        assert!(!frame.text().contains("queued text"));
        assert!(!frame.text().contains("queued reasoning"));
        assert!(!frame.text().contains("queued draft"));
    }

    #[test]
    fn exec_continuations_share_the_frame_boundary_queue() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.tui.start_exec("printf test".into());
        let block_id = app
            .tui
            .conversation
            .transcript()
            .history()
            .last_block_id()
            .expect("exec block");

        app.tui.append_exec_output("first".into());
        app.tui.append_exec_output(" second".into());
        let smelt_transcript::transcript_model::Block::Exec { output, .. } = app
            .tui
            .conversation
            .transcript()
            .history()
            .block(block_id)
            .expect("exec block")
        else {
            panic!("last block is exec output");
        };
        assert!(output.is_empty());

        app.tui.finish_exec(None);
        assert!(app
            .tui
            .render_pending_transient_frame_to(&mut std::io::sink()));

        let history = app.tui.conversation.transcript().history();
        let smelt_transcript::transcript_model::Block::Exec { output, .. } =
            history.block(block_id).expect("completed exec block")
        else {
            panic!("last block is exec output");
        };
        assert_eq!(output.snapshot(), "first\n second");
        assert_eq!(
            history.status(block_id),
            Some(smelt_transcript::transcript_model::Status::Done)
        );
    }

    #[test]
    fn animation_only_frames_refresh_prompt_top_without_reprojecting_transcript() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.start_turn(1);
        app.dispatch_engine_event(protocol::EngineEvent::Text {
            content: "retained transcript".into(),
        });
        app.run_lua_result(
            r#"
            _G.animation_renderer_calls = 0
            smelt.prompt.top.win:set_renderer(function(win)
              _G.animation_renderer_calls = _G.animation_renderer_calls + 1
              win:buf():lines({ tostring(smelt.signal.get("work_elapsed_ms")) })
              smelt.timer.set(16, function() win:invalidate() end)
            end)
            "#,
        )
        .expect("install animation renderer probe");

        app.render_normal_to(&mut std::io::sink());
        let projection_count = app
            .tui
            .conversation
            .transcript()
            .projection_count_for_harness();
        assert_eq!(projection_count, 1, "initial frame must project once");
        let initial_renderer_calls = app
            .lua_int_global("animation_renderer_calls")
            .expect("animation renderer call count");

        for _ in 0..3 {
            app.tui
                .request_animation_render(std::time::Duration::from_millis(16));
            app.clock.advance(std::time::Duration::from_millis(16));
            app.tick_timers();
            assert!(app
                .tui
                .render_requested_transient_frame_to(&mut std::io::sink()));
        }

        assert_eq!(
            app.tui
                .conversation
                .transcript()
                .projection_count_for_harness(),
            projection_count,
            "animation-only frames must not plan or project the transcript"
        );
        assert_eq!(
            app.lua_int_global("animation_renderer_calls"),
            Some(initial_renderer_calls + 3),
            "animation frames must repaint the retained prompt bar"
        );
    }

    #[test]
    fn invalidated_renderer_schedules_its_repaint_once() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.run_lua_result(
            r#"
            smelt.prompt.top.win:set_renderer(function(win)
              smelt.timer.set(16, function() win:invalidate() end)
            end)
            "#,
        )
        .expect("install animated renderer");
        app.tui.schedule_win_repaint();
        app.render_normal_to(&mut std::io::sink());
        app.tui.schedule_win_repaint();
        assert_eq!(app.tui.scheduled_frame_delay(), None);

        app.clock.advance(std::time::Duration::from_millis(16));
        app.tui.tick_runtime();
        app.tui.schedule_win_repaint();
        assert!(
            app.tui.scheduled_frame_delay().is_some(),
            "a renderer invalidated outside a frame must schedule its repaint"
        );

        app.render_normal_to(&mut std::io::sink());
        app.tui.schedule_win_repaint();
        assert_eq!(app.tui.scheduled_frame_delay(), None);
    }

    #[test]
    fn due_declarative_refresh_reprojects_retained_elapsed_header() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.start_turn(1);
        app.tool_started(
            "timer-call",
            "read_file",
            std::collections::HashMap::from([(
                "file_path".to_string(),
                serde_json::json!("timer.rs"),
            )]),
        );

        app.clock.advance(std::time::Duration::from_millis(1_000));
        let initial = app.render_to_frame();
        assert!(initial.text().contains("1.0s"));
        let initial_projection_count = app
            .tui
            .conversation
            .transcript()
            .projection_count_for_harness();

        app.clock.advance(std::time::Duration::from_millis(50));
        let before_deadline = app.render_to_frame();
        assert!(before_deadline.text().contains("1.0s"));
        assert_eq!(
            app.tui
                .conversation
                .transcript()
                .projection_count_for_harness(),
            initial_projection_count,
            "elapsed animation before the declared refresh deadline must reuse the row tape"
        );

        app.clock.advance(std::time::Duration::from_millis(50));
        let at_deadline = app.render_to_frame();
        assert!(at_deadline.text().contains("1.1s"));
        assert_eq!(
            app.tui
                .conversation
                .transcript()
                .projection_count_for_harness(),
            initial_projection_count + 1,
            "a due declarative refresh must rebuild the retained transcript projection"
        );
    }

    #[test]
    fn lua_window_renderers_retain_content_until_invalidated_or_resized() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.run_lua_result(
            r#"
            local buf = smelt.buf.new({ name = "retained-renderer-test" })
            local win = smelt.win.new(buf, { name = "retained-renderer-test" })
            _G.retained_renderer_calls = 0
            win:set_renderer(function()
              _G.retained_renderer_calls = _G.retained_renderer_calls + 1
              buf:lines({ tostring(_G.retained_renderer_calls) })
            end)
            _G.retained_renderer_win = win
            _G.retained_renderer_id = tonumber(tostring(win):match("(%d+)$"))
            "#,
        )
        .expect("register retained window renderer");

        app.render_silent();
        assert_eq!(app.lua_int_global("retained_renderer_calls"), Some(0));
        let win_id = app
            .lua_int_global("retained_renderer_id")
            .expect("retained renderer window id") as u64;
        assert!(
            app.tui
                .lua
                .shared()
                .win_renderers
                .lock()
                .expect("renderer registry")
                .get(&win_id)
                .expect("retained renderer")
                .dirty,
            "an unmounted renderer must remain dirty"
        );

        app.run_lua_result(
            r#"
            smelt.ui.layout.set(function()
              return smelt.ui.layout.leaf(retained_renderer_win)
            end)
            "#,
        )
        .expect("mount retained renderer window");
        app.render_silent();
        assert_eq!(app.lua_int_global("retained_renderer_calls"), Some(1));
        app.render_silent();
        assert_eq!(app.lua_int_global("retained_renderer_calls"), Some(1));

        app.run_lua_result("retained_renderer_win:invalidate()")
            .expect("invalidate retained window renderer");
        app.render_silent();
        assert_eq!(app.lua_int_global("retained_renderer_calls"), Some(2));

        let (width, height) = app.ui_probe().terminal_size();
        app.set_terminal_size(width + 10, height);
        app.render_silent();
        assert_eq!(app.lua_int_global("retained_renderer_calls"), Some(3));
        app.run_lua_result("smelt.ui.layout.invalidate()").unwrap();
        app.render_silent();
        assert_eq!(
            app.lua_int_global("retained_renderer_calls"),
            Some(3),
            "recomputing an unchanged layout must retain the rendered content"
        );

        app.run_lua_result("smelt.ui.layout.set(nil)").unwrap();
        app.render_silent();
        assert_eq!(app.lua_int_global("retained_renderer_calls"), Some(3));
        app.run_lua_result(
            r#"
            smelt.ui.layout.set(function()
                return smelt.ui.layout.leaf(retained_renderer_win)
            end)
        "#,
        )
        .unwrap();
        app.render_silent();
        assert_eq!(app.lua_int_global("retained_renderer_calls"), Some(4));
    }

    #[test]
    fn lua_prompt_text_replacement_invalidates_prompt_aux() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.run_lua_result(
            r#"
            _G.prompt_aux_renderer_calls = 0
            smelt.prompt.aux.win:set_renderer(function()
              _G.prompt_aux_renderer_calls = _G.prompt_aux_renderer_calls + 1
            end)
            smelt.ui.layout.set(function()
              return smelt.ui.layout.leaf(smelt.prompt.aux.win)
            end)
            "#,
        )
        .expect("install retained prompt text probe");

        app.render_silent();
        assert_eq!(app.lua_int_global("prompt_aux_renderer_calls"), Some(1));
        app.render_silent();
        assert_eq!(app.lua_int_global("prompt_aux_renderer_calls"), Some(1));

        app.run_lua_result(r#"smelt.prompt.set_text("changed")"#)
            .expect("replace prompt text");
        app.render_silent();
        assert_eq!(app.lua_int_global("prompt_aux_renderer_calls"), Some(2));
    }

    #[test]
    fn prompt_queue_revision_invalidates_queue_and_layout() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.start_turn(1);
        app.run_lua_result(
            r#"
            _G.prompt_queue_renderer_calls = 0
            _G.prompt_queue_layout_calls = 0
            smelt.prompt.queue.win:set_renderer(function()
              _G.prompt_queue_renderer_calls = _G.prompt_queue_renderer_calls + 1
            end)
            smelt.ui.layout.set(function()
              _G.prompt_queue_layout_calls = _G.prompt_queue_layout_calls + 1
              return (smelt.prompt.queue:node())
            end)
            "#,
        )
        .expect("install retained prompt queue probes");

        app.render_silent();
        assert_eq!(app.lua_int_global("prompt_queue_renderer_calls"), Some(1));
        assert_eq!(app.lua_int_global("prompt_queue_layout_calls"), Some(1));
        app.render_silent();
        assert_eq!(app.lua_int_global("prompt_queue_renderer_calls"), Some(1));
        assert_eq!(app.lua_int_global("prompt_queue_layout_calls"), Some(1));

        assert!(app
            .tui
            .prompt
            .try_queue_turn(crate::app::QueuedInput::request_from_text(
                "queued", "queued", 0
            )));
        app.render_silent();
        assert_eq!(app.lua_int_global("prompt_queue_layout_calls"), Some(2));
        assert_eq!(app.lua_int_global("prompt_queue_renderer_calls"), Some(2));
        app.render_silent();
        assert_eq!(app.lua_int_global("prompt_queue_layout_calls"), Some(2));
        assert_eq!(app.lua_int_global("prompt_queue_renderer_calls"), Some(2));
    }

    #[test]
    fn lua_main_layout_runs_only_after_dimension_change_or_invalidation() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.run_lua_result(
            r#"
            _G.retained_layout_calls = 0
            smelt.ui.layout.set(function()
              _G.retained_layout_calls = _G.retained_layout_calls + 1
              return smelt.ui.layout.leaf(smelt.transcript.win)
            end)
            "#,
        )
        .expect("register retained main layout");

        app.render_silent();
        assert_eq!(app.lua_int_global("retained_layout_calls"), Some(1));
        app.render_silent();
        assert_eq!(app.lua_int_global("retained_layout_calls"), Some(1));

        app.run_lua_result("smelt.ui.layout.invalidate()")
            .expect("invalidate retained main layout");
        app.render_silent();
        assert_eq!(app.lua_int_global("retained_layout_calls"), Some(2));

        app.tui.handle_resize(120, 40);
        assert_eq!(app.lua_int_global("retained_layout_calls"), Some(3));
    }

    #[test]
    fn turn_completing_inside_a_frame_paints_the_finished_work_state() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.start_turn(1);
        app.render_to_frame();
        assert!(app.render_to_frame().text().contains('\u{25CF}'));

        app.dispatch_engine_event(protocol::EngineEvent::TextDelta {
            delta: "answer".into(),
        });
        app.tui
            .queue_engine_continuation(protocol::EngineEvent::TextDelta {
                delta: " tail".into(),
            });
        app.dispatch_engine_event(protocol::EngineEvent::TurnComplete {
            turn_id: 1,
            history: None,
            meta: None,
        });
        assert!(app.agent_running());

        let mut sink = std::io::sink();
        app.render_normal_to(&mut sink);
        assert!(!app.agent_running());
        let state: String = app
            .eval_lua(r#"return smelt.signal.get("work_state")"#)
            .expect("work_state");
        assert_eq!(state, "done");
        assert!(app.tui.ui.flushed_snapshot().text().contains("done"));
    }
}
