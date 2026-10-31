//! Scroll geometry of resumed sparse transcripts whose rendered heights differ
//! a lot from the store's estimates (diffs render taller, collapsed exec output
//! renders shorter).

use super::*;
use crossterm::event::{MouseButton, MouseEventKind};

fn mouse(kind: MouseEventKind, x: u16, y: u16) -> SourceEvent {
    SourceEvent::Term(Event::Mouse(crossterm::event::MouseEvent {
        kind,
        row: y,
        column: x,
        modifiers: KeyModifiers::empty(),
    }))
}

fn record_marker(line: &str) -> Option<usize> {
    let start = line.find("record-")? + "record-".len();
    let digits: String = line[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// Block padding and gaps leave at most a few blank rows at the bottom of a
/// page. A hole in the viewport's backing leaves most of it blank.
const MAX_BLOCK_GAP_ROWS: usize = 4;

struct View {
    first_record: Option<usize>,
    last_record: Option<usize>,
    blank_tail: usize,
    has_scrollbar: bool,
}

fn view(app: &TestApp) -> View {
    let lines = transcript_viewport_lines(app);
    View {
        first_record: lines.iter().find_map(|line| record_marker(line)),
        last_record: lines
            .iter()
            .filter_map(|line| record_marker(line))
            .next_back(),
        blank_tail: lines
            .iter()
            .rev()
            .take_while(|line| line.trim().is_empty())
            .count(),
        has_scrollbar: app
            .transcript_window()
            .viewport
            .is_some_and(|vp| vp.scrollbar.is_some()),
    }
}

fn diff_records(count: usize) -> Vec<smelt_core::TranscriptBlockRecord> {
    use smelt_core::transcript_model::{
        Block, ToolOutput, ToolOutputContentField, ToolState, ToolStatus,
    };
    let mut source = smelt_core::content::transcript::Transcript::new();
    for idx in 0..count {
        let marker = format!("record-{idx:04}");
        match idx % 4 {
            0 => source.push(Block::User {
                text: format!(
                    "{marker} please edit {}",
                    "soft wrapped request ".repeat(10)
                ),
                image_labels: vec![],
                command: false,
                sent_at_ms: None,
            }),
            1 => source.push(Block::Text {
                content: format!("{marker} answer {}", "wrapping prose ".repeat(30)).into(),
            }),
            _ => {
                let lines = 10 + (idx * 7) % 40;
                let body = |name: &str, fill: char, step: usize| -> String {
                    (0..lines)
                        .map(|l| {
                            let pad = fill.to_string().repeat((l * step) % 150);
                            format!("    let {name}_{l} = {marker} {pad};\n")
                        })
                        .collect()
                };
                let (old, new) = (body("old", 'x', 13), body("new", 'y', 11));
                let path = format!("src/{marker}.rs");
                let (name, args, fields) = if idx % 4 == 2 {
                    (
                        "edit_file",
                        std::collections::HashMap::from([
                            ("file_path".to_string(), serde_json::json!(path)),
                            ("old_string".to_string(), serde_json::json!(old)),
                            ("new_string".to_string(), serde_json::json!(new)),
                        ]),
                        vec![
                            ToolOutputContentField {
                                name: "old_content".into(),
                                content: old.into(),
                            },
                            ToolOutputContentField {
                                name: "new_content".into(),
                                content: new.into(),
                            },
                        ],
                    )
                } else {
                    (
                        "write_file",
                        std::collections::HashMap::from([
                            ("file_path".to_string(), serde_json::json!(path)),
                            ("content".to_string(), serde_json::json!(new)),
                        ]),
                        vec![],
                    )
                };
                source.push_tool_call(
                    Block::ToolCall {
                        call_id: format!("call-{idx}"),
                        name: name.into(),
                        summary: protocol::StyledLines::from_plain(format!(
                            "{marker} {name} {path}"
                        )),
                        args: args.into(),
                    },
                    ToolState {
                        status: ToolStatus::Ok,
                        elapsed: Some(Duration::from_millis(5)),
                        called_at_ms: None,
                        elapsed_active: false,
                        output: Some(Box::new(ToolOutput {
                            content: format!("{marker} wrote {path}").into(),
                            is_error: false,
                            metadata: Some(serde_json::json!({ "path": path })),
                            content_fields: fields,
                        })),
                        user_message: None,
                        preview_output: None,
                    },
                );
            }
        }
    }
    source.history.block_records()
}

fn exec_records(count: usize) -> Vec<smelt_core::TranscriptBlockRecord> {
    use smelt_core::transcript_model::Block;
    let mut source = smelt_core::content::transcript::Transcript::new();
    for idx in 0..count {
        let marker = format!("record-{idx:04}");
        match idx % 3 {
            0 => source.push(Block::User {
                text: format!("{marker} run it"),
                image_labels: vec![],
                command: false,
                sent_at_ms: None,
            }),
            1 => source.push(Block::Exec {
                command: format!("echo {marker}"),
                output: (0..120)
                    .map(|l| format!("{marker} out {l}\n"))
                    .collect::<String>()
                    .into(),
            }),
            _ => source.push(Block::Text {
                content: format!("{marker} done").into(),
            }),
        }
    }
    source.history.block_records()
}

/// Alternating runs of under- and over-estimated records.
fn mixed_records(count: usize) -> Vec<smelt_core::TranscriptBlockRecord> {
    let (diff, exec) = (diff_records(count), exec_records(count));
    (0..count)
        .map(|idx| {
            if (idx / 40) % 2 == 0 {
                diff[idx].clone()
            } else {
                exec[idx].clone()
            }
        })
        .collect()
}

fn resumed(records: Vec<smelt_core::TranscriptBlockRecord>) -> (TestApp, tempfile::TempDir) {
    super::misc::resumed_transcript_app_from_records(records, 120, 40)
}

/// Drag the thumb from the top of the track to past its bottom, returning the
/// view after every drag step.
fn drag_thumb_down(app: &mut TestApp) -> Vec<View> {
    let vp = app.transcript_window().viewport.expect("viewport");
    let bar = vp.scrollbar.expect("scrollbar");
    let thumb = bar.metrics(app.transcript_window().scroll_top);
    let grab = thumb.thumb_top + thumb.thumb_size / 2;
    app.feed_one(mouse(
        MouseEventKind::Down(MouseButton::Left),
        bar.col,
        vp.rect.y + grab,
    ));
    app.render_silent();
    let mut views = Vec::new();
    for row in 0..vp.rect.height + 3 {
        app.feed_one(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            bar.col,
            vp.rect.y + row,
        ));
        app.render_silent();
        views.push(view(app));
    }
    app.feed_one(mouse(
        MouseEventKind::Up(MouseButton::Left),
        bar.col,
        vp.rect.bottom() + 2,
    ));
    app.render_silent();
    views
}

fn click_track(app: &mut TestApp, rel_row: u16) {
    let vp = app.transcript_window().viewport.expect("viewport");
    let bar = vp.scrollbar.expect("scrollbar");
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        app.feed_one(mouse(kind, bar.col, vp.rect.y + rel_row));
        app.render_silent();
    }
}

fn assert_drag_sweeps_the_transcript(records: Vec<smelt_core::TranscriptBlockRecord>) {
    let count = records.len();
    let (mut app, _dir) = resumed(records);
    let views = drag_thumb_down(&mut app);

    let firsts: Vec<usize> = views.iter().filter_map(|view| view.first_record).collect();
    assert_eq!(firsts.len(), views.len(), "every drag step shows records");
    assert!(
        firsts.windows(2).all(|pair| pair[0] <= pair[1]),
        "dragging down never moves content up: {firsts:?}"
    );
    let mut distinct = firsts.clone();
    distinct.dedup();
    assert!(
        distinct.len() * 2 >= views.len(),
        "drag steps reach distinct positions instead of locking: {firsts:?}"
    );
    assert!(
        firsts[0] < count / 20,
        "track top shows the start: {firsts:?}"
    );
    for (step, view) in views.iter().enumerate() {
        assert!(view.has_scrollbar, "scrollbar vanished at step {step}");
        assert!(
            view.blank_tail <= MAX_BLOCK_GAP_ROWS,
            "step {step} left {} blank rows below the content",
            view.blank_tail
        );
    }
    let last = views.last().expect("drag views");
    assert_eq!(
        last.last_record,
        Some(count - 1),
        "track bottom shows the end"
    );
}

#[test]
fn thumb_drag_sweeps_underestimated_diff_transcripts() {
    assert_drag_sweeps_the_transcript(diff_records(602));
}

#[test]
fn thumb_drag_sweeps_overestimated_exec_transcripts() {
    assert_drag_sweeps_the_transcript(exec_records(600));
}

#[test]
fn track_clicks_near_the_end_fill_the_viewport() {
    let (mut app, _dir) = resumed(diff_records(602));
    let height = app
        .transcript_window()
        .viewport
        .expect("viewport")
        .rect
        .height;
    for rel_row in [height / 2, height * 3 / 4, height - 2, height - 1] {
        click_track(&mut app, rel_row);
        let view = view(&app);
        assert!(
            view.has_scrollbar,
            "scrollbar vanished after clicking {rel_row}"
        );
        assert!(
            view.blank_tail <= MAX_BLOCK_GAP_ROWS,
            "clicking {rel_row} left {} blank rows below the content",
            view.blank_tail
        );
    }
}

/// Wheel `ticks` times and return how often the top record moved against the
/// wheel direction or jumped further than a few records.
fn wheel_glitches(app: &mut TestApp, down: bool, ticks: usize) -> Vec<(usize, usize)> {
    let vp = app.transcript_window().viewport.expect("viewport");
    let kind = if down {
        MouseEventKind::ScrollDown
    } else {
        MouseEventKind::ScrollUp
    };
    let mut previous = view(app).first_record;
    let mut glitches = Vec::new();
    for _ in 0..ticks {
        app.feed_one(mouse(kind, 5, vp.rect.y + 5));
        app.render_silent();
        let first = view(app).first_record;
        if let (Some(before), Some(after)) = (previous, first) {
            let reversed = if down { after < before } else { after > before };
            if reversed || after.abs_diff(before) > 6 {
                glitches.push((before, after));
            }
        }
        previous = first.or(previous);
    }
    glitches
}

#[test]
fn wheel_scrolling_moves_steadily_through_mixed_transcripts() {
    let (mut app, _dir) = resumed(mixed_records(600));
    assert_eq!(wheel_glitches(&mut app, false, 400), vec![]);
    click_track(&mut app, 0);
    assert_eq!(wheel_glitches(&mut app, true, 400), vec![]);
    click_track(&mut app, 17);
    assert_eq!(wheel_glitches(&mut app, true, 300), vec![]);
    assert_eq!(wheel_glitches(&mut app, false, 300), vec![]);
}
