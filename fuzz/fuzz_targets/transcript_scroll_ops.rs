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
use tui::app::test_harness::{
    TranscriptScrollProbeCommand, TranscriptScrollProbeEdge,
};

#[derive(Arbitrary, Debug)]
struct Input {
    descriptors: u16,
    fixture: u8,
    width: u8,
    height: u8,
    ops: Vec<Op>,
}

#[derive(Arbitrary, Debug)]
enum Op {
    Wheel { down: bool, row: u8, ticks: u8 },
    Click { row: u8, col: u8 },
    DragSelect { from_row: u8, to_row: u8, col: u8 },
    StartEdgeDrag { bottom: bool },
    DragAutoscroll { ticks: u8 },
    FinishDrag,
    Command { kind: u8, repeat: u8 },
    Search { descriptor: u16 },
    Reveal { descriptor: u16 },
    Scrollbar { row: u8 },
    Resize { width: u8, height: u8 },
    Append { variant: u8 },
    FollowTail,
    NoInputRender,
    Render,
}

fn run(input: Input) {
    with_current_thread_runtime("transcript_scroll_ops", || run_with_app(input));
}

fn run_with_app(input: Input) {
    let heavy_fixture = input.fixture % 16 == 0;
    let descriptor_count = if heavy_fixture {
        256 + usize::from(input.descriptors % 512)
    } else {
        96 + usize::from(input.descriptors % 96)
    };
    let op_limit = if heavy_fixture { 128 } else { 48 };
    let width = u16::from(input.width % 96).saturating_add(40);
    let height = u16::from(input.height % 28).saturating_add(10);
    let mut app = TestApp::builder().with_vim(true).build();
    app.install_sparse_transcript_scroll_fixture(descriptor_count, width, height);

    for op in input.ops.into_iter().take(op_limit) {
        match op {
            Op::Wheel { down, row, ticks } => {
                for _ in 0..ticks.min(8).max(1) {
                    app.transcript_scroll_probe_wheel(down, u16::from(row));
                    app.transcript_scroll_probe_render();
                }
                continue;
            }
            Op::Click { row, col } => {
                app.transcript_scroll_probe_content_click(u16::from(row), u16::from(col));
            }
            Op::DragSelect { from_row, to_row, col } => {
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
                for _ in 0..ticks.min(16).max(1) {
                    let _ = app.transcript_scroll_probe_drag_autoscroll_tick();
                    app.transcript_scroll_probe_render();
                }
                continue;
            }
            Op::FinishDrag => app.transcript_scroll_probe_finish_drag(),
            Op::Command { kind, repeat } => {
                let command = command(kind);
                for _ in 0..repeat.min(12).max(1) {
                    app.transcript_scroll_probe_command(command);
                    app.transcript_scroll_probe_render();
                }
                continue;
            }
            Op::Search { descriptor } => {
                app.transcript_scroll_probe_search_record(usize::from(descriptor));
            }
            Op::Reveal { descriptor } => {
                app.transcript_scroll_probe_reveal_descriptor(usize::from(descriptor));
            }
            Op::Scrollbar { row } => app.transcript_scroll_probe_scrollbar_click(u16::from(row)),
            Op::Resize { width, height } => {
                let width = u16::from(width % 96).saturating_add(32);
                let height = u16::from(height % 32).saturating_add(8);
                app.set_terminal_size(width, height);
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
