use super::*;

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use smelt_core::transcript_model::{Block, TranscriptBlockRecord};

use crate::app::search::SearchDirection;
use crate::app::transcript::TranscriptDocument;
use crate::app::transcript_scroll_trace::{
    TranscriptDescriptorTraceRange, TranscriptProjectionTargetTrace, TranscriptScrollIntent,
    TranscriptScrollTraceFrame, TranscriptTraceAnchor,
};
use crate::smelt_edit::VimMode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptScrollProbeEdge {
    Top,
    Bottom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptScrollProbeCommand {
    MoveDown,
    MoveUp,
    PageDown,
    PageUp,
    HalfPageDown,
    HalfPageUp,
    JumpTop,
    JumpBottom,
}

static SPARSE_FIXTURE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Clone, Copy, Debug)]
struct UserDeltaAnchor {
    sign: i8,
    virtual_row: u64,
}

#[derive(Default)]
pub(super) struct TranscriptScrollProbeState {
    drag_edge: Option<TranscriptScrollProbeEdge>,
    last_user_delta_anchor: Option<UserDeltaAnchor>,
    fixture: Option<SparseTranscriptFixture>,
}

impl TranscriptScrollProbeState {
    fn keep_fixture_alive(&self) {
        if let Some(fixture) = &self.fixture {
            let _ = fixture.path();
        }
    }
}

struct SparseTranscriptFixture {
    session_dir: std::path::PathBuf,
}

impl SparseTranscriptFixture {
    fn new(session_dir: std::path::PathBuf) -> Self {
        let _ = std::fs::remove_dir_all(&session_dir);
        std::fs::create_dir_all(&session_dir).expect("create transcript fixture dir");
        Self { session_dir }
    }

    fn path(&self) -> &std::path::Path {
        &self.session_dir
    }
}

impl Drop for SparseTranscriptFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.session_dir);
    }
}

impl TestApp {
    pub fn install_sparse_transcript_scroll_fixture(
        &mut self,
        descriptor_count: usize,
        width: u16,
        height: u16,
    ) {
        let descriptor_count = descriptor_count.clamp(96, 900);
        let width = width.clamp(32, 140);
        let height = height.clamp(8, 40);
        let records = heterogeneous_resume_records(descriptor_count);
        let fixture_id = SPARSE_FIXTURE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let fixture = SparseTranscriptFixture::new(
            managed_harness_dir("transcript-scroll").join(format!("fixture-{fixture_id}")),
        );
        crate::persist::write_transcript_descriptor_suffix(fixture.path(), 0, &records)
            .expect("write descriptor suffix");
        let loaded = crate::app::transcript::LoadedTranscript::tail_from_sqlite_dir(
            fixture.path().to_path_buf(),
            width,
            height,
        )
        .expect("tail transcript");

        self.set_terminal_size(width, height);
        self.app.session_document.transcript = TranscriptDocument::from_loaded_transcript(loaded);
        self.app.app_focus = AppFocus::Content;
        self.app.ui.set_focus(crate::app::TRANSCRIPT_WIN);
        self.app.transcript_win_mut().set_vim_enabled(true);
        self.app
            .transcript_win_mut()
            .set_vim_mode(crate::smelt_edit::VimMode::Normal);
        self.app.transcript_win_mut().follow_tail();
        self.app
            .session_document
            .transcript
            .set_scroll_trace_enabled(true);
        self.transcript_scroll_probe = TranscriptScrollProbeState {
            fixture: Some(fixture),
            ..TranscriptScrollProbeState::default()
        };
        self.render_silent();
        self.app
            .session_document
            .transcript
            .take_scroll_trace_frames();
    }

    pub fn transcript_scroll_probe_render(&mut self) {
        self.transcript_scroll_probe.keep_fixture_alive();
        self.render_silent();
        let frames = self
            .app
            .session_document
            .transcript
            .take_scroll_trace_frames();
        assert_transcript_scroll_probe_frames(&mut self.transcript_scroll_probe, &frames);
        self.assert_invariants();
    }

    pub fn transcript_scroll_probe_no_input_render(&mut self) {
        let before_scroll = self.app.transcript_win().scroll_top();
        self.transcript_scroll_probe_render();
        assert_eq!(
            self.app.transcript_win().scroll_top(),
            before_scroll,
            "no-input render changed transcript scroll_top"
        );
    }

    pub fn transcript_scroll_probe_wheel(&mut self, down: bool, rel_row: u16) {
        let (row, col) = self.transcript_scroll_probe_content_point(rel_row, 1);
        let kind = if down {
            MouseEventKind::ScrollDown
        } else {
            MouseEventKind::ScrollUp
        };
        self.feed_one(SourceEvent::Term(crossterm::event::Event::Mouse(
            MouseEvent {
                kind,
                row,
                column: col,
                modifiers: KeyModifiers::empty(),
            },
        )));
    }

    pub fn transcript_scroll_probe_content_click(&mut self, rel_row: u16, rel_col: u16) {
        let (row, col) = self.transcript_scroll_probe_content_point(rel_row, rel_col);
        self.feed_one(SourceEvent::Term(crossterm::event::Event::Mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                row,
                column: col,
                modifiers: KeyModifiers::empty(),
            },
        )));
    }

    pub fn transcript_scroll_probe_drag_select(
        &mut self,
        from_row: u16,
        to_row: u16,
        rel_col: u16,
    ) {
        let (start_row, col) = self.transcript_scroll_probe_content_point(from_row, rel_col);
        let (end_row, _) = self.transcript_scroll_probe_content_point(to_row, rel_col);
        self.feed_one(SourceEvent::Term(crossterm::event::Event::Mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                row: start_row,
                column: col,
                modifiers: KeyModifiers::empty(),
            },
        )));
        self.feed_one(SourceEvent::Term(crossterm::event::Event::Mouse(
            MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                row: end_row,
                column: col,
                modifiers: KeyModifiers::empty(),
            },
        )));
    }

    pub fn transcript_scroll_probe_start_edge_drag(&mut self, edge: TranscriptScrollProbeEdge) {
        let vp = self
            .app
            .transcript_win()
            .viewport
            .expect("transcript viewport");
        let col = vp
            .rect
            .left
            .saturating_add(vp.gutter_width)
            .saturating_add(1);
        let down_row = vp.rect.top.saturating_add(vp.rect.height / 2);
        let edge_row = match edge {
            TranscriptScrollProbeEdge::Top => vp.rect.top,
            TranscriptScrollProbeEdge::Bottom => vp.rect.bottom().saturating_sub(1),
        };
        self.feed_one(SourceEvent::Term(crossterm::event::Event::Mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                row: down_row,
                column: col,
                modifiers: KeyModifiers::empty(),
            },
        )));
        self.feed_one(SourceEvent::Term(crossterm::event::Event::Mouse(
            MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                row: edge_row,
                column: col,
                modifiers: KeyModifiers::empty(),
            },
        )));
        self.transcript_scroll_probe.drag_edge = Some(edge);
    }

    pub fn transcript_scroll_probe_drag_autoscroll_tick(&mut self) -> bool {
        self.app.tick_drag_autoscroll_with_transcript_intent()
    }

    pub fn transcript_scroll_probe_finish_drag(&mut self) {
        let (row, col) = self.transcript_scroll_probe_content_point(1, 1);
        self.feed_one(SourceEvent::Term(crossterm::event::Event::Mouse(
            MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                row,
                column: col,
                modifiers: KeyModifiers::empty(),
            },
        )));
        self.transcript_scroll_probe.drag_edge = None;
    }

    pub fn transcript_scroll_probe_scrollbar_click(&mut self, rel_row: u16) {
        let Some(vp) = self.app.transcript_win().viewport else {
            return;
        };
        let Some(scrollbar) = vp.scrollbar else {
            return;
        };
        let row = vp
            .rect
            .top
            .saturating_add(rel_row.min(vp.rect.height.saturating_sub(1)));
        self.feed_one(SourceEvent::Term(crossterm::event::Event::Mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                row,
                column: scrollbar.col,
                modifiers: KeyModifiers::empty(),
            },
        )));
        self.feed_one(SourceEvent::Term(crossterm::event::Event::Mouse(
            MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                row,
                column: scrollbar.col,
                modifiers: KeyModifiers::empty(),
            },
        )));
    }

    pub fn transcript_scroll_probe_command(&mut self, command: TranscriptScrollProbeCommand) {
        self.app.app_focus = AppFocus::Content;
        self.app.ui.set_focus(crate::app::TRANSCRIPT_WIN);
        self.app.transcript_win_mut().set_vim_enabled(true);
        self.app.transcript_win_mut().set_vim_mode(VimMode::Normal);
        match command {
            TranscriptScrollProbeCommand::MoveDown => self.press(KeyCode::Down),
            TranscriptScrollProbeCommand::MoveUp => self.press(KeyCode::Up),
            TranscriptScrollProbeCommand::PageDown => self.press(KeyCode::PageDown),
            TranscriptScrollProbeCommand::PageUp => self.press(KeyCode::PageUp),
            TranscriptScrollProbeCommand::HalfPageDown => {
                self.press_mod(KeyCode::Char('d'), KeyModifiers::CONTROL)
            }
            TranscriptScrollProbeCommand::HalfPageUp => {
                self.press_mod(KeyCode::Char('u'), KeyModifiers::CONTROL)
            }
            TranscriptScrollProbeCommand::JumpTop => {
                self.type_char('g');
                self.type_char('g');
            }
            TranscriptScrollProbeCommand::JumpBottom => self.type_char('G'),
        }
    }

    pub fn transcript_scroll_probe_reveal_descriptor(&mut self, descriptor_index: usize) {
        let total = self
            .app
            .session_document
            .transcript
            .descriptor_total_count()
            .unwrap_or(1)
            .max(1);
        let descriptor_index = descriptor_index % total;
        let _ = self
            .app
            .reveal_transcript_descriptor_block(descriptor_index, 1, true);
    }

    pub fn transcript_scroll_probe_search_record(&mut self, descriptor_index: usize) {
        let total = self
            .app
            .session_document
            .transcript
            .descriptor_total_count()
            .unwrap_or(1)
            .max(1);
        let descriptor_index = descriptor_index % total;
        self.app.submit_search(
            crate::app::TRANSCRIPT_WIN,
            SearchDirection::Forward,
            format!("record-{descriptor_index:04}"),
        );
    }

    pub fn transcript_scroll_probe_append(&mut self, variant: u8) {
        let marker = format!("fuzz-append-{variant:03}");
        let content = match variant % 4 {
            0 => format!("{marker} assistant append {}", "tail ".repeat(16)),
            1 => format!("{marker}\n\n```rust\nfn appended() {{}}\n```"),
            2 => format!("{marker} markdown paragraph {}", "wrap ".repeat(32)),
            _ => format!("{marker} compact-ish summary {}", "summary ".repeat(12)),
        };
        self.app.push_block(Block::Text { content });
    }

    pub fn transcript_scroll_probe_follow_tail(&mut self) {
        self.app.transcript_win_mut().follow_tail();
    }

    fn transcript_scroll_probe_content_point(&self, rel_row: u16, rel_col: u16) -> (u16, u16) {
        let vp = self
            .app
            .transcript_win()
            .viewport
            .expect("transcript viewport");
        let row = vp
            .rect
            .top
            .saturating_add(rel_row.min(vp.rect.height.saturating_sub(1)));
        let col = vp
            .rect
            .left
            .saturating_add(vp.gutter_width)
            .saturating_add(rel_col.min(vp.content_width.saturating_sub(1)));
        (row, col)
    }
}

fn assert_transcript_scroll_probe_frames(
    state: &mut TranscriptScrollProbeState,
    frames: &[TranscriptScrollTraceFrame],
) {
    for frame in frames {
        match &frame.scroll_intent {
            TranscriptScrollIntent::UserDelta { rows } => {
                assert_eq!(
                    frame.window_scroll_after_input, frame.window_scroll_before,
                    "local transcript input mutated Window::scroll_top before projection: {frame:?}"
                );
                assert!(
                    matches!(
                        frame.projection_target,
                        TranscriptProjectionTargetTrace::ExactRow(_)
                    ),
                    "local transcript movement projected through a non-exact target: {frame:?}"
                );
                assert!(
                    !frame.placeholder_rows_visible,
                    "local transcript movement exposed sparse placeholders: {frame:?}"
                );
                assert!(
                    frame.first_visible_content_anchor.is_some(),
                    "local transcript movement lost its visible content anchor: {frame:?}"
                );
                assert_descriptor_ranges_overlap(state, frame);
                assert_user_delta_direction(state, *rows, frame);
            }
            TranscriptScrollIntent::SearchJump { .. }
            | TranscriptScrollIntent::RevealBlock { .. } => {
                assert!(
                    !frame.placeholder_rows_visible,
                    "semantic transcript reveal exposed sparse placeholders: {frame:?}"
                );
                assert!(
                    frame.first_visible_content_anchor.is_some(),
                    "semantic transcript reveal did not resolve visible content: {frame:?}"
                );
                state.last_user_delta_anchor = None;
            }
            TranscriptScrollIntent::ExactContentAnchor(anchor) => {
                assert!(
                    !matches!(anchor, TranscriptTraceAnchor::EstimatedRow(_)),
                    "exact transcript operation fell back to an estimated row anchor: {frame:?}"
                );
                state.last_user_delta_anchor = None;
            }
            TranscriptScrollIntent::PreserveViewport
            | TranscriptScrollIntent::ResizeReflow { .. } => {
                assert_preserve_frame_keeps_anchor(frame);
                state.last_user_delta_anchor = None;
            }
            TranscriptScrollIntent::ScrollbarFraction { .. }
            | TranscriptScrollIntent::ApproximateRowSeek(_)
            | TranscriptScrollIntent::Tail
            | TranscriptScrollIntent::PageDelta { .. } => {
                if !frame.placeholder_rows_visible {
                    assert!(
                        frame.first_visible_content_anchor.is_some(),
                        "resolved transcript frame has no visible content anchor: {frame:?}"
                    );
                }
                state.last_user_delta_anchor = None;
            }
        }
    }
}

fn assert_descriptor_ranges_overlap(
    state: &TranscriptScrollProbeState,
    frame: &TranscriptScrollTraceFrame,
) {
    let Some(edge) = state.drag_edge else {
        return;
    };
    let Some(before) = frame.active_descriptor_range_before else {
        return;
    };
    let Some(after) = frame.active_descriptor_range_after else {
        return;
    };
    assert!(
        ranges_overlap(before, after),
        "{edge:?} drag/autoscroll jumped to disjoint descriptor coverage: before={before:?}, after={after:?}, frame={frame:?}"
    );
}

fn ranges_overlap(a: TranscriptDescriptorTraceRange, b: TranscriptDescriptorTraceRange) -> bool {
    a.start <= b.end && b.start <= a.end
}

fn assert_user_delta_direction(
    state: &mut TranscriptScrollProbeState,
    rows: isize,
    frame: &TranscriptScrollTraceFrame,
) {
    let sign = rows.signum() as i8;
    let Some(anchor) = frame.first_visible_content_anchor else {
        state.last_user_delta_anchor = None;
        return;
    };
    let virtual_row = anchor.virtual_row;
    let Some(previous) = state.last_user_delta_anchor else {
        state.last_user_delta_anchor = Some(UserDeltaAnchor { sign, virtual_row });
        return;
    };
    if previous.sign == sign && sign < 0 {
        assert!(
            virtual_row <= previous.virtual_row,
            "upward local movement moved visible content downward: previous={}, current={virtual_row}, frame={frame:?}",
            previous.virtual_row
        );
    } else if previous.sign == sign && sign > 0 {
        assert!(
            virtual_row >= previous.virtual_row,
            "downward local movement moved visible content upward: previous={}, current={virtual_row}, frame={frame:?}",
            previous.virtual_row
        );
    }
    state.last_user_delta_anchor = Some(UserDeltaAnchor { sign, virtual_row });
}

fn assert_preserve_frame_keeps_anchor(frame: &TranscriptScrollTraceFrame) {
    let Some(TranscriptTraceAnchor::Content {
        descriptor_index: before_descriptor,
        block_id: before_block,
        ..
    }) = frame.viewport_anchor_before
    else {
        return;
    };
    let Some(TranscriptTraceAnchor::Content {
        descriptor_index: after_descriptor,
        block_id: after_block,
        ..
    }) = frame.viewport_anchor_after
    else {
        panic!("preserve/resize frame lost content anchor: {frame:?}");
    };
    assert_eq!(
        (after_descriptor, after_block),
        (before_descriptor, before_block),
        "preserve/resize frame moved to different content identity: {frame:?}"
    );
}

fn heterogeneous_resume_records(count: usize) -> Vec<TranscriptBlockRecord> {
    let mut source = smelt_core::content::transcript::Transcript::new();
    for idx in 0..count {
        let marker = format!("record-{idx:04}");
        match idx % 10 {
            0 => source.push(Block::User {
                text: format!(
                    "{marker} user prompt with image labels and wrapped text {}",
                    "u ".repeat(12)
                ),
                image_labels: vec![format!("image-{idx}")],
            }),
            1 => source.push(Block::Text {
                content: format!(
                    "{marker} assistant paragraph\n\n```diff\n- old {idx}\n+ new {idx}\n```\n{}",
                    "markdown wrap ".repeat(20)
                ),
            }),
            2 => source.push(Block::Thinking {
                content: format!("{marker} thinking trace {}", "reasoning ".repeat(28)),
            }),
            3 => source.push(Block::CodeLine {
                content: format!("{marker} let value_{idx} = compute({idx});"),
                lang: "rust".into(),
            }),
            4 => source.push(Block::Exec {
                command: format!("echo {marker}"),
                output: format!("{marker} stdout line\n{}", "exec output ".repeat(18)),
            }),
            5 => source.push(Block::Compacted {
                summary: format!("{marker} compacted summary {}", "summary ".repeat(10)),
            }),
            6 => source.push(Block::CompactionPreview {
                summary: format!("{marker} streaming preview {}", "preview ".repeat(15)),
            }),
            7 => source.push(Block::ToolCall {
                call_id: format!("read-file-{idx}"),
                name: "read_file".into(),
                summary: protocol::StyledLines::from_plain(format!(
                    "{marker} read_file src/{idx}.rs"
                )),
                args: std::collections::HashMap::from([(
                    "file_path".to_string(),
                    serde_json::json!(format!("src/{idx}.rs")),
                )]),
            }),
            8 => source.push(Block::ToolCall {
                call_id: format!("grep-{idx}"),
                name: "grep".into(),
                summary: protocol::StyledLines::from_plain(format!("{marker} grep needle")),
                args: std::collections::HashMap::from([(
                    "pattern".to_string(),
                    serde_json::json!(marker),
                )]),
            }),
            _ => source.push(Block::ProcessStatus {
                text: format!("{marker} background process finished"),
                event: None,
            }),
        };
    }
    source.history.descriptor_records()
}
