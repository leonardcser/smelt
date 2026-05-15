#![no_main]

//! Drive `TestApp` through arbitrary `SourceEvent` streams and assert
//! that core text-buffer + registry invariants always hold.
//!
//! Each fuzz iteration:
//!   1. Snapshots baseline sizes of process-global interners.
//!   2. Builds a fresh `TestApp` (vim toggled by the first input byte).
//!   3. Translates a sequence of `FuzzOp` values into `SourceEvent`s.
//!   4. Feeds each event under the default per-event allocation budget
//!      and runs `TestApp::assert_invariants` after every event.
//!   5. After teardown, confirms the static interners did not grow past a
//!      small slack — a runaway intern leak shows up as a strictly growing
//!      baseline across libFuzzer iterations.

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use tui::app::test_harness::{AllocBudget, SourceEvent, TestApp};

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

/// Slack allowed on top of the post-build interner baseline. Some scenarios
/// legitimately intern a few new highlight groups (picker rows, dialog
/// styles) — the leak signal is unbounded growth, not a handful of new ids.
const INTERN_SLACK: usize = 64;

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

fuzz_target!(|input: FuzzInput| {
    let mut app = TestApp::builder().with_vim(input.vim).build();
    // Snapshot AFTER build so theme/namespace setup done by autoload counts as
    // baseline. Any growth past `baseline + INTERN_SLACK` during the scenario
    // is the leak signal.
    let theme_baseline = smelt_style::theme::registry_len();
    let ns_baseline = smelt_buffer::buffer::namespace_count();

    let ops = if input.ops.len() > MAX_OPS {
        &input.ops[..MAX_OPS]
    } else {
        &input.ops[..]
    };
    for op in ops {
        let ev = translate(op);
        app.feed_one_within_budget(ev, AllocBudget::DEFAULT);
        app.assert_invariants();
        if app.quit_requested() {
            break;
        }
    }

    let theme_end = smelt_style::theme::registry_len();
    let ns_end = smelt_buffer::buffer::namespace_count();
    assert!(
        theme_end <= theme_baseline + INTERN_SLACK,
        "theme registry leaked: {} -> {} (slack {})",
        theme_baseline,
        theme_end,
        INTERN_SLACK
    );
    assert!(
        ns_end <= ns_baseline + INTERN_SLACK,
        "namespace registry leaked: {} -> {} (slack {})",
        ns_baseline,
        ns_end,
        INTERN_SLACK
    );
});
