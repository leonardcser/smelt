//! Debug-facing transcript scroll contract and disabled-by-default trace schema.
//!
//! The transcript scroll model treats user input as semantic intent over
//! content, not as a durable assignment to `Window::scroll_top`. The trace uses
//! that vocabulary so tests and local reproductions can explain each frame.
//!
//! Contract:
//!
//! - Wheel deltas are local content movement. A wheel tick requests movement by
//!   a fixed number of content rows from the current visible anchor. Repeated
//!   ticks must not speed up, slow down, or reverse because unloaded extent
//!   estimates refined while the gesture was in flight.
//! - Drag autoscroll is the same local content movement as wheel scrolling,
//!   driven by edge ticks while selection state remains in document
//!   coordinates. Each tick advances by the configured content-row amount until
//!   a real content boundary is reached.
//! - Scrollbar clicks and drags are coarse far-seek intents. They may use stable
//!   unloaded record estimates to choose an initial record window, but
//!   the resolved viewport must be re-anchored to exact materialized content
//!   before text, selection, actions, or hit testing are exposed.
//! - Tail-follow is a mode, not a request for the latest numeric total row. It
//!   remains pinned to the semantic tail while appends and extent observations
//!   update the scrollbar projection.
//! - Resize and reflow preserve the visible content anchor and row bias across
//!   width changes. They do not reinterpret the previous `scroll_top` as a new
//!   exact row in a different layout.
//! - Estimate refinement can update scrollbar totals and coarse seek math, but
//!   it cannot move visible content. Exact observations gathered during a frame
//!   must leave the top visible content anchor stable unless the user supplied a
//!   new intent or the viewport reached a real boundary.
//!
//! Trace records intentionally use record indices, row anchors, block ids,
//! counts, and optional timings. They must not contain transcript text.

use std::ops::Range;

use serde_json::json;

use crate::app::transcript::TranscriptSearchAnchor;
use crate::content::transcript_scene::RenderNodeId;
use crate::smelt_edit::{add_signed_row, DocPosition, RowIndex};
use smelt_core::transcript_model::BlockId;

#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TranscriptScrollIntent {
    Tail,
    PreserveViewport,
    UserDelta {
        rows: isize,
    },
    PageDelta {
        pages: isize,
    },
    ExactContentAnchor(TranscriptTraceAnchor),
    SearchJump {
        anchor: TranscriptSearchAnchor,
        target_screen_row: RowIndex,
        match_start_byte_col: usize,
        match_end_byte_col: usize,
    },
    RevealBlock {
        record_index: usize,
        block_id: BlockId,
        row_offset: RowIndex,
        screen_padding_top: RowIndex,
        cursor_byte_col: Option<usize>,
    },
    RevealFirstRecord {
        screen_padding_top: RowIndex,
        cursor_byte_col: Option<usize>,
    },
    ResizeReflow {
        previous_width: u16,
    },
    ScrollbarFraction {
        numerator: u64,
        denominator: u64,
        total_rows: RowIndex,
        viewport_rows: u16,
    },
    ApproximateRowSeek(RowIndex),
    RevealPosition {
        anchor: TranscriptTraceAnchor,
        target_screen_row: RowIndex,
        cursor_position: Option<DocPosition>,
    },
}

impl TranscriptScrollIntent {
    pub(crate) fn is_explicit_tail_intent(&self) -> bool {
        match self {
            Self::Tail => true,
            Self::ScrollbarFraction {
                numerator,
                denominator,
                ..
            } => *numerator >= (*denominator).max(1),
            Self::ApproximateRowSeek(_) => true,
            Self::PreserveViewport
            | Self::UserDelta { .. }
            | Self::PageDelta { .. }
            | Self::ExactContentAnchor(_)
            | Self::RevealPosition { .. }
            | Self::SearchJump { .. }
            | Self::RevealBlock { .. }
            | Self::RevealFirstRecord { .. }
            | Self::ResizeReflow { .. } => false,
        }
    }

    pub(crate) fn may_repin_when_semantic_tail_reached(&self) -> bool {
        match self {
            Self::UserDelta { rows } => *rows > 0,
            Self::PageDelta { pages } => *pages > 0,
            _ => self.is_explicit_tail_intent(),
        }
    }

    pub(crate) fn is_downward_local_delta(&self) -> bool {
        match self {
            Self::UserDelta { rows } => *rows > 0,
            Self::PageDelta { pages } => *pages > 0,
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TranscriptRecordTraceRange {
    pub(crate) start: usize,
    pub(crate) end: usize,
}

impl TranscriptRecordTraceRange {
    pub(crate) fn from_store_range(range: &Range<smelt_store::TranscriptRecordOffset>) -> Self {
        Self {
            start: range.start.get(),
            end: range.end.get(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TranscriptRowTraceRange {
    pub(crate) start: RowIndex,
    pub(crate) end: RowIndex,
}

impl From<Range<RowIndex>> for TranscriptRowTraceRange {
    fn from(range: Range<RowIndex>) -> Self {
        Self {
            start: range.start,
            end: range.end,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TranscriptTraceAnchor {
    Tail,
    Content {
        virtual_row: RowIndex,
        record_index: usize,
        block_id: BlockId,
        node_id: RenderNodeId,
        row_offset: RowIndex,
    },
    EstimatedRow(RowIndex),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TranscriptProjectionTargetTrace {
    Tail,
    ExactRow(RowIndex),
    StableRowDelta { row: RowIndex, delta: isize },
    ReflowStableRow(RowIndex),
}

impl TranscriptProjectionTargetTrace {
    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn exact_target_row(self) -> Option<RowIndex> {
        match self {
            Self::ExactRow(row) => Some(row),
            Self::StableRowDelta { row, delta } => Some(add_signed_row(row, delta)),
            Self::Tail | Self::ReflowStableRow(_) => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TranscriptVisibleContentAnchor {
    pub(crate) virtual_row: RowIndex,
    pub(crate) node_id: RenderNodeId,
    pub(crate) row_offset: RowIndex,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TranscriptContentMovement {
    ComparableRows {
        rows: i128,
    },
    DifferentContent {
        from: TranscriptVisibleContentAnchor,
        to: TranscriptVisibleContentAnchor,
    },
    MissingAnchor,
}

#[allow(dead_code)]
pub(crate) fn compare_visible_content_movement(
    before: Option<TranscriptVisibleContentAnchor>,
    after: Option<TranscriptVisibleContentAnchor>,
) -> TranscriptContentMovement {
    match (before, after) {
        (Some(before), Some(after)) if before.node_id == after.node_id => {
            TranscriptContentMovement::ComparableRows {
                rows: i128::from(after.row_offset) - i128::from(before.row_offset),
            }
        }
        (Some(before), Some(after)) => TranscriptContentMovement::DifferentContent {
            from: before,
            to: after,
        },
        _ => TranscriptContentMovement::MissingAnchor,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TranscriptScrollTraceFrame {
    pub(crate) retained: bool,
    pub(crate) input_event_or_tick: String,
    pub(crate) scroll_intent: TranscriptScrollIntent,
    pub(crate) window_scroll_before: RowIndex,
    pub(crate) window_scroll_after_input: RowIndex,
    pub(crate) viewport_anchor_before: Option<TranscriptTraceAnchor>,
    pub(crate) projection_target: TranscriptProjectionTargetTrace,
    pub(crate) active_record_range_before: Option<TranscriptRecordTraceRange>,
    pub(crate) prefix_estimate_before: RowIndex,
    pub(crate) suffix_estimate_before: RowIndex,
    pub(crate) exact_observation_count: usize,
    pub(crate) resolved_scroll_top: RowIndex,
    pub(crate) viewport_anchor_after: Option<TranscriptTraceAnchor>,
    pub(crate) active_record_range_after: Option<TranscriptRecordTraceRange>,
    pub(crate) materialized_range: TranscriptRowTraceRange,
    pub(crate) placeholder_rows_visible: bool,
    pub(crate) first_visible_content_anchor: Option<TranscriptVisibleContentAnchor>,
    pub(crate) last_visible_content_anchor: Option<TranscriptVisibleContentAnchor>,
    pub(crate) visible_record_or_block_ids: Vec<BlockId>,
    pub(crate) render_or_projection_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TranscriptScrollTraceRenderInput {
    pub(crate) input_event_or_tick: String,
    pub(crate) scroll_intent: TranscriptScrollIntent,
    pub(crate) window_scroll_before: RowIndex,
    pub(crate) window_scroll_after_input: RowIndex,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TranscriptScrollTraceFrameStart {
    pub(crate) retained: bool,
    pub(crate) input: TranscriptScrollTraceRenderInput,
    pub(crate) viewport_anchor_before: Option<TranscriptTraceAnchor>,
    pub(crate) projection_target: TranscriptProjectionTargetTrace,
    pub(crate) active_record_range_before: Option<TranscriptRecordTraceRange>,
    pub(crate) prefix_estimate_before: RowIndex,
    pub(crate) suffix_estimate_before: RowIndex,
    pub(crate) exact_observation_count: usize,
}

impl TranscriptScrollTraceFrameStart {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn finish(
        self,
        resolved_scroll_top: RowIndex,
        viewport_anchor_after: Option<TranscriptTraceAnchor>,
        active_record_range_after: Option<TranscriptRecordTraceRange>,
        materialized_range: Range<RowIndex>,
        placeholder_rows_visible: bool,
        first_visible_content_anchor: Option<TranscriptVisibleContentAnchor>,
        last_visible_content_anchor: Option<TranscriptVisibleContentAnchor>,
        visible_record_or_block_ids: Vec<BlockId>,
        render_or_projection_ms: Option<u64>,
    ) -> TranscriptScrollTraceFrame {
        TranscriptScrollTraceFrame {
            retained: self.retained,
            input_event_or_tick: self.input.input_event_or_tick,
            scroll_intent: self.input.scroll_intent,
            window_scroll_before: self.input.window_scroll_before,
            window_scroll_after_input: self.input.window_scroll_after_input,
            viewport_anchor_before: self.viewport_anchor_before,
            projection_target: self.projection_target,
            active_record_range_before: self.active_record_range_before,
            prefix_estimate_before: self.prefix_estimate_before,
            suffix_estimate_before: self.suffix_estimate_before,
            exact_observation_count: self.exact_observation_count,
            resolved_scroll_top,
            viewport_anchor_after,
            active_record_range_after,
            materialized_range: materialized_range.into(),
            placeholder_rows_visible,
            first_visible_content_anchor,
            last_visible_content_anchor,
            visible_record_or_block_ids,
            render_or_projection_ms,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TranscriptInteractionTraceEvent {
    pub(crate) seq: u64,
    pub(crate) kind: String,
    pub(crate) data: serde_json::Value,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TranscriptScrollTrace {
    frames: Vec<TranscriptScrollTraceFrame>,
    interaction_events: Vec<TranscriptInteractionTraceEvent>,
    pending_input: Option<TranscriptScrollTraceRenderInput>,
    last_resolved_scroll_top: Option<RowIndex>,
    record_timings: bool,
    next_event_seq: u64,
}

impl TranscriptScrollTrace {
    #[allow(dead_code)]
    pub(crate) fn with_timings(record_timings: bool) -> Self {
        Self {
            record_timings,
            ..Self::default()
        }
    }

    pub(crate) fn record_interaction(&mut self, kind: impl Into<String>, data: serde_json::Value) {
        let event = TranscriptInteractionTraceEvent {
            seq: self.next_event_seq,
            kind: kind.into(),
            data,
        };
        self.next_event_seq = self.next_event_seq.saturating_add(1);
        self.interaction_events.push(event);
    }

    #[allow(dead_code)]
    pub(crate) fn interaction_events(&self) -> &[TranscriptInteractionTraceEvent] {
        &self.interaction_events
    }

    #[allow(dead_code)]
    pub(crate) fn take_interaction_events(&mut self) -> Vec<TranscriptInteractionTraceEvent> {
        std::mem::take(&mut self.interaction_events)
    }

    pub(crate) fn record_frame_event(&mut self, frame: &TranscriptScrollTraceFrame) {
        let kind = if frame.retained {
            "retained_frame"
        } else {
            "projection_frame"
        };
        self.record_interaction(kind, trace_frame_json(frame));
    }

    pub(crate) fn record_timings(&self) -> bool {
        self.record_timings
    }

    pub(crate) fn set_pending_input(&mut self, input: TranscriptScrollTraceRenderInput) {
        self.pending_input = Some(input);
    }

    pub(crate) fn has_pending_input(&self) -> bool {
        self.pending_input.is_some()
    }

    pub(crate) fn clear_pending_input(&mut self) {
        self.pending_input = None;
    }

    pub(crate) fn replace_pending_intent(&mut self, intent: TranscriptScrollIntent) {
        if let Some(input) = self.pending_input.as_mut() {
            input.scroll_intent = intent;
        }
    }

    pub(crate) fn take_pending_input_or_default(
        &mut self,
        projection_target: TranscriptProjectionTargetTrace,
    ) -> TranscriptScrollTraceRenderInput {
        self.pending_input.take().unwrap_or_else(|| {
            let row = match projection_target {
                TranscriptProjectionTargetTrace::Tail => 0,
                TranscriptProjectionTargetTrace::ExactRow(row)
                | TranscriptProjectionTargetTrace::ReflowStableRow(row) => row,
                TranscriptProjectionTargetTrace::StableRowDelta { row, delta } => {
                    add_signed_row(row, delta)
                }
            };
            TranscriptScrollTraceRenderInput {
                input_event_or_tick: "render_frame".to_string(),
                scroll_intent: match projection_target {
                    TranscriptProjectionTargetTrace::Tail => TranscriptScrollIntent::Tail,
                    TranscriptProjectionTargetTrace::ExactRow(row) => {
                        TranscriptScrollIntent::ApproximateRowSeek(row)
                    }
                    TranscriptProjectionTargetTrace::StableRowDelta { .. }
                    | TranscriptProjectionTargetTrace::ReflowStableRow(_) => {
                        TranscriptScrollIntent::PreserveViewport
                    }
                },
                window_scroll_before: self.last_resolved_scroll_top.unwrap_or(row),
                window_scroll_after_input: row,
            }
        })
    }

    pub(crate) fn push(&mut self, frame: TranscriptScrollTraceFrame) {
        self.last_resolved_scroll_top = Some(frame.resolved_scroll_top);
        self.record_frame_event(&frame);
        self.frames.push(frame);
    }

    pub(crate) fn last_resolved_scroll_top(&self) -> Option<RowIndex> {
        self.last_resolved_scroll_top
    }

    #[allow(dead_code)]
    pub(crate) fn frames(&self) -> &[TranscriptScrollTraceFrame] {
        &self.frames
    }

    #[allow(dead_code)]
    pub(crate) fn take_frames(&mut self) -> Vec<TranscriptScrollTraceFrame> {
        std::mem::take(&mut self.frames)
    }
}

fn trace_frame_json(frame: &TranscriptScrollTraceFrame) -> serde_json::Value {
    json!({
        "retained": frame.retained,
        "input_event_or_tick": &frame.input_event_or_tick,
        "scroll_intent": format!("{:?}", frame.scroll_intent),
        "window_scroll_before": frame.window_scroll_before,
        "window_scroll_after_input": frame.window_scroll_after_input,
        "viewport_anchor_before": trace_option_debug(frame.viewport_anchor_before),
        "projection_target": format!("{:?}", frame.projection_target),
        "active_record_range_before": frame.active_record_range_before.map(trace_record_range_json),
        "prefix_estimate_before": frame.prefix_estimate_before,
        "suffix_estimate_before": frame.suffix_estimate_before,
        "exact_observation_count": frame.exact_observation_count,
        "resolved_scroll_top": frame.resolved_scroll_top,
        "viewport_anchor_after": trace_option_debug(frame.viewport_anchor_after),
        "active_record_range_after": frame.active_record_range_after.map(trace_record_range_json),
        "materialized_range": trace_row_range_json(frame.materialized_range),
        "placeholder_rows_visible": frame.placeholder_rows_visible,
        "first_visible_content_anchor": frame.first_visible_content_anchor.map(trace_visible_anchor_json),
        "last_visible_content_anchor": frame.last_visible_content_anchor.map(trace_visible_anchor_json),
        "visible_record_or_block_ids": frame
            .visible_record_or_block_ids
            .iter()
            .map(|id| format!("{:?}", id))
            .collect::<Vec<_>>(),
        "render_or_projection_ms": frame.render_or_projection_ms,
    })
}

fn trace_record_range_json(range: TranscriptRecordTraceRange) -> serde_json::Value {
    json!({ "start": range.start, "end": range.end })
}

fn trace_row_range_json(range: TranscriptRowTraceRange) -> serde_json::Value {
    json!({ "start": range.start, "end": range.end })
}

fn trace_visible_anchor_json(anchor: TranscriptVisibleContentAnchor) -> serde_json::Value {
    json!({
        "virtual_row": anchor.virtual_row,
        "node_id": format!("{:?}", anchor.node_id),
        "row_offset": anchor.row_offset,
    })
}

fn trace_option_debug<T: std::fmt::Debug>(value: Option<T>) -> Option<String> {
    value.map(|value| format!("{value:?}"))
}
