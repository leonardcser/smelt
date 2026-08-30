#![no_main]

//! Focused sparse transcript interaction fuzzing. This target starts from a
//! resumed heterogeneous transcript and drives the user-interaction surfaces that
//! are hard to cover through generic app fuzzing: local wheel movement, cursor
//! commands, selection dragging, edge autoscroll, sparse search/reveal jumps,
//! scrollbar seeking, resize/reflow, tail-follow, appends, and no-input renders.
//! Most inputs use a compact sparse fixture for throughput; a small fraction use
//! a larger sparse resume to keep deep virtualization paths covered.

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use smelt_fuzz::{runtime::with_current_thread_runtime, TestApp};
use tui::app::test_harness::{TranscriptScrollProbeCommand, TranscriptScrollProbeEdge};

#[derive(Arbitrary, Debug)]
struct Input {
    records: u16,
    fixture: u8,
    width: u8,
    height: u8,
    ops: Vec<Op>,
}

#[derive(Arbitrary, Debug)]
enum Op {
    Wheel { down: bool, row: u8, ticks: u8 },
    WheelBurst { down: bool, row: u8, ticks: u8 },
    MixedWheelBurst { down: bool, row: u8, ticks: u8 },
    Click { row: u8, col: u8 },
    DragSelect { from_row: u8, to_row: u8, col: u8 },
    StartEdgeDrag { bottom: bool },
    DragAutoscroll { ticks: u8 },
    FinishDrag,
    Command { kind: u8, repeat: u8 },
    Search { record: u16 },
    Reveal { record: u16 },
    Scrollbar { row: u8 },
    Resize { width: u8, height: u8 },
    Append { variant: u8 },
    FollowTail,
    NoInputRender,
    Render,
    SearchCommon,
    RepeatSearch { reverse: bool },
    StartScrollbarDrag { row: u8 },
    DragScrollbar { row: u8 },
    FinishScrollbarDrag { row: u8 },
    UnsettledRender,
}

fn run(input: Input) {
    with_current_thread_runtime("transcript_scroll_ops", || run_with_app(input));
}

fn run_with_app(input: Input) {
    let heavy_fixture = input.fixture.is_multiple_of(16);
    let record_count = if heavy_fixture {
        256 + usize::from(input.records % 512)
    } else {
        96 + usize::from(input.records % 96)
    };
    let op_limit = if heavy_fixture { 128 } else { 48 };
    let width = u16::from(input.width % 96).saturating_add(40);
    let height = u16::from(input.height % 28).saturating_add(10);
    let mut app = TestApp::builder().with_vim(true).build();
    app.install_sparse_transcript_scroll_fixture(record_count, width, height);

    for op in input.ops.into_iter().take(op_limit) {
        match op {
            Op::Wheel { down, row, ticks } => {
                for _ in 0..ticks.clamp(1, 8) {
                    app.transcript_scroll_probe_wheel(down, u16::from(row));
                    app.transcript_scroll_probe_render();
                }
                continue;
            }
            Op::WheelBurst { down, row, ticks } => {
                for _ in 0..ticks.clamp(1, 2) {
                    app.transcript_scroll_probe_wheel(down, u16::from(row));
                }
                app.transcript_scroll_probe_render();
                continue;
            }
            Op::MixedWheelBurst {
                mut down,
                row,
                ticks,
            } => {
                for _ in 0..ticks.clamp(2, 4) {
                    app.transcript_scroll_probe_wheel(down, u16::from(row));
                    down = !down;
                }
                app.transcript_scroll_probe_render();
                continue;
            }
            Op::Click { row, col } => {
                app.transcript_scroll_probe_content_click(u16::from(row), u16::from(col));
            }
            Op::DragSelect {
                from_row,
                to_row,
                col,
            } => {
                app.transcript_scroll_probe_drag_select(
                    u16::from(from_row),
                    u16::from(to_row),
                    u16::from(col),
                );
            }
            Op::StartEdgeDrag { bottom } => {
                let edge = if bottom {
                    TranscriptScrollProbeEdge::Bottom
                } else {
                    TranscriptScrollProbeEdge::Top
                };
                app.transcript_scroll_probe_start_edge_drag(edge);
            }
            Op::DragAutoscroll { ticks } => {
                for _ in 0..ticks.clamp(1, 16) {
                    let _ = app.transcript_scroll_probe_drag_autoscroll_tick();
                    app.transcript_scroll_probe_render();
                }
                continue;
            }
            Op::FinishDrag => app.transcript_scroll_probe_finish_drag(),
            Op::Command { kind, repeat } => {
                let command = command(kind);
                for _ in 0..repeat.clamp(1, 12) {
                    app.transcript_scroll_probe_command(command);
                    app.transcript_scroll_probe_render();
                }
                continue;
            }
            Op::Search { record } => {
                app.transcript_scroll_probe_search_record(usize::from(record));
            }
            Op::SearchCommon => app.transcript_scroll_probe_search_common_text(),
            Op::RepeatSearch { reverse } => {
                app.transcript_scroll_probe_repeat_search(reverse);
            }
            Op::Reveal { record } => {
                app.transcript_scroll_probe_reveal_record(usize::from(record));
            }
            Op::Scrollbar { row } => app.transcript_scroll_probe_scrollbar_click(u16::from(row)),
            Op::StartScrollbarDrag { row } => {
                app.transcript_scroll_probe_start_scrollbar_drag(u16::from(row));
                app.render_unsettled_silent();
                continue;
            }
            Op::DragScrollbar { row } => {
                app.transcript_scroll_probe_drag_scrollbar(u16::from(row));
                app.render_unsettled_silent();
                continue;
            }
            Op::FinishScrollbarDrag { row } => {
                app.transcript_scroll_probe_finish_scrollbar_drag(u16::from(row));
                app.render_unsettled_silent();
                continue;
            }
            Op::UnsettledRender => {
                app.render_unsettled_silent();
                continue;
            }
            Op::Resize { width, height } => {
                let width = u16::from(width % 96).saturating_add(32);
                let height = u16::from(height % 32).saturating_add(8);
                app.transcript_scroll_probe_resize(width, height);
            }
            Op::Append { variant } => app.transcript_scroll_probe_append(variant),
            Op::FollowTail => app.transcript_scroll_probe_follow_tail(),
            Op::NoInputRender => {
                app.transcript_scroll_probe_no_input_render();
                continue;
            }
            Op::Render => {}
        }
        app.transcript_scroll_probe_render();
    }
}

fn command(kind: u8) -> TranscriptScrollProbeCommand {
    match kind % 8 {
        0 => TranscriptScrollProbeCommand::MoveDown,
        1 => TranscriptScrollProbeCommand::MoveUp,
        2 => TranscriptScrollProbeCommand::PageDown,
        3 => TranscriptScrollProbeCommand::PageUp,
        4 => TranscriptScrollProbeCommand::HalfPageDown,
        5 => TranscriptScrollProbeCommand::HalfPageUp,
        6 => TranscriptScrollProbeCommand::JumpTop,
        _ => TranscriptScrollProbeCommand::JumpBottom,
    }
}

fuzz_target!(|input: Input| run(input));
