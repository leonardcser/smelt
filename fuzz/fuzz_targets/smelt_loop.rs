#![no_main]

//! Drive `TestApp` through arbitrary `SourceEvent` streams and assert
//! that core text-buffer invariants always hold.
//!
//! Each fuzz iteration:
//!   1. Builds a fresh `TestApp` (vim toggled by the first input byte).
//!   2. Translates a sequence of `FuzzOp` values into `SourceEvent`s.
//!   3. Feeds each event under the default per-event allocation budget,
//!      so runaway-allocation regressions fail the corpus entry.
//!   4. After every event, checks that the prompt buffer's source is
//!      still valid UTF-8 and its cursor sits on a char boundary
//!      inside `[0, source.len()]`.

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use tui::app::test_harness::{AllocBudget, SourceEvent, TestApp};
use tui::app::{PROMPT_EDIT_BUF, PROMPT_WIN};

use crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers,
};

#[derive(Arbitrary, Debug)]
enum FuzzOp {
    /// Printable ASCII keystroke (byte is mapped into the printable range).
    KeyChar(u8),
    /// Control-modified ASCII letter.
    KeyCtrl(u8),
    /// Bare special key chosen by `which % SPECIALS_LEN`.
    KeySpecial(u8),
    /// Advance the virtual clock by `ms` milliseconds.
    Tick(u16),
    /// Wake any pending Lua callbacks.
    LuaWakeup,
    /// Resize event using the current width/height.
    Resize,
}

#[derive(Arbitrary, Debug)]
struct FuzzInput {
    vim: bool,
    ops: Vec<FuzzOp>,
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
];

const MAX_OPS: usize = 256;

fn key_event(code: KeyCode, mods: KeyModifiers) -> TermEvent {
    TermEvent::Key(KeyEvent {
        code,
        modifiers: mods,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    })
}

fn translate(op: &FuzzOp) -> SourceEvent {
    match op {
        FuzzOp::KeyChar(b) => {
            // Map into printable ASCII [0x20, 0x7e].
            let c = (0x20u8 + (b % 0x5f)) as char;
            SourceEvent::Term(key_event(KeyCode::Char(c), KeyModifiers::NONE))
        }
        FuzzOp::KeyCtrl(b) => {
            let c = (b'a' + (b % 26)) as char;
            SourceEvent::Term(key_event(KeyCode::Char(c), KeyModifiers::CONTROL))
        }
        FuzzOp::KeySpecial(which) => {
            let code = SPECIALS[(*which as usize) % SPECIALS.len()];
            SourceEvent::Term(key_event(code, KeyModifiers::NONE))
        }
        FuzzOp::Tick(ms) => SourceEvent::Tick(u64::from(*ms)),
        FuzzOp::LuaWakeup => SourceEvent::LuaWakeup,
        FuzzOp::Resize => SourceEvent::Resize,
    }
}

fn assert_prompt_invariants(app: &TestApp) {
    let source = app
        .app
        .ui
        .buf(PROMPT_EDIT_BUF)
        .map(|b| b.source().to_string())
        .unwrap_or_default();

    // Cursor must sit on a UTF-8 char boundary inside the source bytes.
    if let Some(win) = app.app.ui.win(PROMPT_WIN) {
        let cpos = win.cpos;
        assert!(
            cpos <= source.len(),
            "prompt cpos {} out of bounds (source len {})",
            cpos,
            source.len()
        );
        let snapped = smelt_buffer::text::snap(&source, cpos);
        assert_eq!(
            snapped, cpos,
            "prompt cpos {} not on a UTF-8 char boundary (snapped to {})",
            cpos, snapped
        );
    }
}

fuzz_target!(|input: FuzzInput| {
    let mut app = TestApp::builder().with_vim(input.vim).build();
    let ops = if input.ops.len() > MAX_OPS {
        &input.ops[..MAX_OPS]
    } else {
        &input.ops[..]
    };
    for op in ops {
        let ev = translate(op);
        app.feed_one_within_budget(ev, AllocBudget::DEFAULT);
        assert_prompt_invariants(&app);
        if app.quit_requested() {
            break;
        }
    }
});
