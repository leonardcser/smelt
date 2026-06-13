#![no_main]

//! Direct edit/window fuzzing. This is the low-dependency counterpart to
//! `smelt_loop`: it drives `smelt_edit::Ui`, buffers, window layout, vim key
//! handling, mouse dispatch, resize, and rendering without the full TUI app.

use arbitrary::Arbitrary;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use libfuzzer_sys::fuzz_target;
use smelt_edit::{BufCreateOpts, Constraint, Event, Gutters, LayoutTree, SplitConfig, Ui};

#[derive(Arbitrary, Debug)]
struct Input {
    vim: bool,
    initial: String,
    ops: Vec<Op>,
}

#[derive(Arbitrary, Debug)]
enum Op {
    Insert { pos: u32, text: String },
    Replace { start: u32, end: u32, text: String },
    SetCursor { pos: u32 },
    KeyChar { ch: u32, ctrl: bool, shift: bool, alt: bool },
    Special { idx: u8, shift: bool },
    Paste(String),
    Mouse { kind: u8, button: u8, row: u8, col: u8 },
    Resize { w: u8, h: u8 },
    ToggleWrap,
    Render,
}

const SPECIALS: &[KeyCode] = &[
    KeyCode::Enter,
    KeyCode::Esc,
    KeyCode::Backspace,
    KeyCode::Tab,
    KeyCode::Up,
    KeyCode::Down,
    KeyCode::Left,
    KeyCode::Right,
    KeyCode::Home,
    KeyCode::End,
    KeyCode::PageUp,
    KeyCode::PageDown,
    KeyCode::Delete,
];

fn run(input: Input) {
    let mut ui = Ui::new();
    ui.set_terminal_size(40, 10);
    let buf = ui.buf_create(BufCreateOpts::default());
    ui.buf_mut(buf).unwrap().set_source(input.initial);
    ui.buf_mut(buf).unwrap().sync_after_edit(40);
    let win = ui
        .win_open_split(
            buf,
            SplitConfig {
                region: "fuzz".into(),
                gutters: Gutters::default(),
            },
        )
        .expect("open fuzz window");
    ui.set_layout(LayoutTree::vbox(vec![(Constraint::Fill, LayoutTree::leaf(win))]));
    ui.set_focus(win);
    if input.vim {
        if let Some(w) = ui.win_mut(win) {
            w.set_vim_enabled(true);
        }
    }

    let take = input.ops.len().min(96);
    for op in input.ops.into_iter().take(take) {
        match op {
            Op::Insert { pos, text } => insert_direct(&mut ui, buf, win, pos as usize, &text),
            Op::Replace { start, end, text } => replace_direct(&mut ui, buf, win, start as usize, end as usize, &text),
            Op::SetCursor { pos } => {
                let source = ui.buf_mut(buf).unwrap().source().to_string();
                let p = smelt_buffer::text::snap(&source, (pos as usize).min(source.len()));
                ui.win_mut(win).unwrap().set_cpos(p);
            }
            Op::KeyChar { ch, ctrl, shift, alt } => {
                let c = char::from_u32(ch).unwrap_or('?');
                dispatch(&mut ui, Event::Key(key(KeyCode::Char(c), mods(ctrl, shift, alt))));
            }
            Op::Special { idx, shift } => {
                let code = SPECIALS[(idx as usize) % SPECIALS.len()];
                dispatch(&mut ui, Event::Key(key(code, mods(false, shift, false))));
            }
            Op::Paste(s) => dispatch(&mut ui, Event::Paste(s)),
            Op::Mouse { kind, button, row, col } => {
                dispatch(
                    &mut ui,
                    Event::Mouse(MouseEvent {
                        kind: mouse_kind(kind, button),
                        row: u16::from(row % 20),
                        column: u16::from(col % 80),
                        modifiers: KeyModifiers::NONE,
                    }),
                );
            }
            Op::Resize { w, h } => {
                let w = u16::from(w % 120).max(1);
                let h = u16::from(h % 40).max(1);
                dispatch(&mut ui, Event::Resize(w, h));
            }
            Op::ToggleWrap => {
                if let Some(w) = ui.win_mut(win) {
                    w.wrap = !w.wrap;
                }
            }
            Op::Render => {
                let _ = ui.snapshot();
            }
        }
        let _ = ui.snapshot();
        assert_window_invariants(&mut ui, buf, win);
    }
}

fn insert_direct(ui: &mut Ui, buf: smelt_edit::BufId, win: smelt_edit::WinId, pos: usize, text: &str) {
    let old_source = ui.buf(buf).unwrap().source().to_string();
    let start = smelt_buffer::text::snap(&old_source, pos);
    let offsets = window_offsets(ui, win);
    {
        let b = ui.buf_mut(buf).unwrap();
        b.text_mut().insert_str(start, text);
        b.sync_after_edit(80);
    }
    repair_window_offsets(ui, buf, win, start, start, text.len(), offsets);
}

fn replace_direct(
    ui: &mut Ui,
    buf: smelt_edit::BufId,
    win: smelt_edit::WinId,
    start: usize,
    end: usize,
    text: &str,
) {
    let old_source = ui.buf(buf).unwrap().source().to_string();
    let start = smelt_buffer::text::snap(&old_source, start);
    let end = smelt_buffer::text::snap(&old_source, end).max(start);
    let offsets = window_offsets(ui, win);
    {
        let b = ui.buf_mut(buf).unwrap();
        b.text_mut().replace_range(start..end, text);
        b.sync_after_edit(80);
    }
    repair_window_offsets(ui, buf, win, start, end, text.len(), offsets);
}

fn window_offsets(ui: &Ui, win: smelt_edit::WinId) -> (usize, Option<usize>) {
    let w = ui.win(win).unwrap();
    (w.cpos(), w.selection_anchor())
}

fn repair_window_offsets(
    ui: &mut Ui,
    buf: smelt_edit::BufId,
    win: smelt_edit::WinId,
    start: usize,
    end: usize,
    inserted_len: usize,
    offsets: (usize, Option<usize>),
) {
    let (cpos, anchor) = offsets;
    let source = ui.buf(buf).unwrap().source().to_string();
    let w = ui.win_mut(win).unwrap();
    w.set_cpos(transform_offset(&source, start, end, inserted_len, cpos));
    w.set_selection_anchor(anchor.map(|offset| transform_offset(&source, start, end, inserted_len, offset)));
    w.set_curswant(None);
}

fn transform_offset(source: &str, start: usize, end: usize, inserted_len: usize, offset: usize) -> usize {
    let removed_len = end.saturating_sub(start);
    let shifted = if offset <= start {
        offset
    } else if offset >= end {
        if inserted_len >= removed_len {
            offset.saturating_add(inserted_len - removed_len)
        } else {
            offset.saturating_sub(removed_len - inserted_len)
        }
    } else {
        start.saturating_add(inserted_len)
    };
    smelt_buffer::text::snap(source, shifted.min(source.len()))
}

fn dispatch(ui: &mut Ui, ev: Event) {
    let mut lua = lua_noop;
    let _ = ui.dispatch_event(ev, &mut lua);
}

fn lua_noop(
    _handle: smelt_edit::LuaHandle,
    _win: smelt_edit::WinId,
    _payload: &smelt_edit::Payload,
) {
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent { code, modifiers, kind: KeyEventKind::Press, state: KeyEventState::NONE }
}

fn mods(ctrl: bool, shift: bool, alt: bool) -> KeyModifiers {
    let mut m = KeyModifiers::NONE;
    if ctrl { m |= KeyModifiers::CONTROL; }
    if shift { m |= KeyModifiers::SHIFT; }
    if alt { m |= KeyModifiers::ALT; }
    m
}

fn mouse_kind(kind: u8, button: u8) -> MouseEventKind {
    let b = match button % 3 {
        0 => MouseButton::Left,
        1 => MouseButton::Right,
        _ => MouseButton::Middle,
    };
    match kind % 8 {
        0 => MouseEventKind::Down(b),
        1 => MouseEventKind::Up(b),
        2 => MouseEventKind::Drag(b),
        3 => MouseEventKind::Moved,
        4 => MouseEventKind::ScrollDown,
        5 => MouseEventKind::ScrollUp,
        6 => MouseEventKind::ScrollLeft,
        _ => MouseEventKind::ScrollRight,
    }
}

fn assert_window_invariants(ui: &mut Ui, buf: smelt_edit::BufId, win: smelt_edit::WinId) {
    let source = ui.buf_mut(buf).unwrap().source().to_string();
    let len = source.len();
    let w = ui.win_mut(win).unwrap();
    assert!(w.cpos() <= len, "cursor beyond source: {} > {len}", w.cpos());
    assert_eq!(smelt_buffer::text::snap(&source, w.cpos()), w.cpos(), "cursor mid-char");
    if let Some(anchor) = w.selection_anchor() {
        assert!(anchor <= len, "selection anchor beyond source: {anchor} > {len}");
        assert_eq!(smelt_buffer::text::snap(&source, anchor), anchor, "selection anchor mid-char");
    }
}

fuzz_target!(|input: Input| run(input));
