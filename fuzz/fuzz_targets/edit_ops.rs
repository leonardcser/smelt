#![no_main]

//! Direct edit/window fuzzing. This is the low-dependency counterpart to
//! `smelt_loop`: it drives `smelt_edit::Ui`, buffers, window layout, vim key
//! handling, mouse dispatch, resize, and rendering without the full TUI app.

use arbitrary::Arbitrary;
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
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
    Insert {
        pos: u32,
        text: String,
    },
    InsertCurated {
        pos: u32,
        grapheme: u8,
    },
    Replace {
        start: u32,
        end: u32,
        text: String,
    },
    ReplaceCurated {
        start: u32,
        end: u32,
        grapheme: u8,
    },
    SetCursor {
        pos: u32,
    },
    KeyChar {
        ch: u32,
        ctrl: bool,
        shift: bool,
        alt: bool,
    },
    Special {
        idx: u8,
        shift: bool,
    },
    Paste(String),
    Mouse {
        kind: u8,
        button: u8,
        row: u8,
        col: u8,
    },
    Resize {
        w: u8,
        h: u8,
    },
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
    ui.buf_mut(buf)
        .unwrap()
        .set_source(plain_text(&input.initial));
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
    ui.set_layout(LayoutTree::vbox(vec![(
        Constraint::Fill,
        LayoutTree::leaf(win),
    )]));
    ui.set_focus(win);
    if input.vim {
        if let Some(w) = ui.win_mut(win) {
            w.set_vim_enabled(true);
        }
    }

    let take = input.ops.len().min(96);
    for op in input.ops.into_iter().take(take) {
        match op {
            Op::Insert { pos, text } => {
                insert_via_dispatch(&mut ui, buf, win, pos as usize, &plain_text(&text))
            }
            Op::InsertCurated { pos, grapheme } => {
                insert_via_dispatch(&mut ui, buf, win, pos as usize, curated_grapheme(grapheme))
            }
            Op::Replace { start, end, text } => replace_via_dispatch(
                &mut ui,
                buf,
                win,
                start as usize,
                end as usize,
                &plain_text(&text),
            ),
            Op::ReplaceCurated {
                start,
                end,
                grapheme,
            } => replace_via_dispatch(
                &mut ui,
                buf,
                win,
                start as usize,
                end as usize,
                curated_grapheme(grapheme),
            ),
            Op::SetCursor { pos } => {
                let source = ui.buf_mut(buf).unwrap().source().to_string();
                let p =
                    smelt_buffer::text::snap_grapheme(&source, (pos as usize).min(source.len()));
                ui.win_mut(win).unwrap().set_cpos(p);
            }
            Op::KeyChar {
                ch,
                ctrl,
                shift,
                alt,
            } => {
                let c = char::from_u32(ch)
                    .filter(|c| *c != smelt_buffer::ATTACHMENT_MARKER)
                    .unwrap_or('?');
                dispatch(
                    &mut ui,
                    Event::Key(key(KeyCode::Char(c), mods(ctrl, shift, alt))),
                );
            }
            Op::Special { idx, shift } => {
                let code = SPECIALS[(idx as usize) % SPECIALS.len()];
                dispatch(&mut ui, Event::Key(key(code, mods(false, shift, false))));
            }
            Op::Paste(s) => dispatch(&mut ui, Event::Paste(plain_text(&s))),
            Op::Mouse {
                kind,
                button,
                row,
                col,
            } => {
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

fn plain_text(s: &str) -> String {
    s.replace(smelt_buffer::ATTACHMENT_MARKER, "")
}

fn curated_grapheme(index: u8) -> &'static str {
    const GRAPHEMES: &[&str] = &[
        "e\u{301}",
        "9\u{fe0f}",
        "👩\u{200d}💻",
        "🇨🇦",
        "\u{600} ",
        "\u{915}\u{94d}\u{937}",
        "👨\u{200d}👩\u{200d}👧\u{200d}👦",
    ];
    GRAPHEMES[index as usize % GRAPHEMES.len()]
}

fn insert_via_dispatch(
    ui: &mut Ui,
    buf: smelt_edit::BufId,
    win: smelt_edit::WinId,
    pos: usize,
    text: &str,
) {
    let source = ui.buf(buf).unwrap().source();
    let pos = pos.min(source.len());
    let window = ui.win_mut(win).unwrap();
    window.set_cpos(pos);
    window.clear_selection_anchor();
    dispatch(ui, Event::Paste(text.to_string()));
}

fn replace_via_dispatch(
    ui: &mut Ui,
    buf: smelt_edit::BufId,
    win: smelt_edit::WinId,
    start: usize,
    end: usize,
    text: &str,
) {
    let source = ui.buf(buf).unwrap().source();
    let start = start.min(source.len());
    let end = end.min(source.len()).max(start);
    let window = ui.win_mut(win).unwrap();
    window.set_selection_anchor(Some(start));
    window.set_cpos(end);
    dispatch(ui, Event::Paste(text.to_string()));
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
    KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

fn mods(ctrl: bool, shift: bool, alt: bool) -> KeyModifiers {
    let mut m = KeyModifiers::NONE;
    if ctrl {
        m |= KeyModifiers::CONTROL;
    }
    if shift {
        m |= KeyModifiers::SHIFT;
    }
    if alt {
        m |= KeyModifiers::ALT;
    }
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
    assert!(
        w.cpos() <= len,
        "cursor beyond source: {} > {len}",
        w.cpos()
    );
    assert_eq!(
        smelt_buffer::text::snap_grapheme(&source, w.cpos()),
        w.cpos(),
        "cursor inside grapheme"
    );
    if let Some(anchor) = w.selection_anchor() {
        assert!(
            anchor <= len,
            "selection anchor beyond source: {anchor} > {len}"
        );
        assert_eq!(
            smelt_buffer::text::snap_grapheme(&source, anchor),
            anchor,
            "selection anchor inside grapheme"
        );
    }
}

fuzz_target!(|input: Input| run(input));
