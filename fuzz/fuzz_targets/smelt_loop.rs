#![no_main]

//! Drive `TestApp` through arbitrary `SourceEvent` streams and assert
//! that core text-buffer + registry invariants always hold.
//!
//! Input shape:
//!   - `vim` toggles the prompt's vim mode at build time.
//!   - `mode` selects the agent mode (Normal/Plan/Apply/Yolo).
//!   - `ops` is the event stream; each op decodes to a single `SourceEvent`.
//!
//! Each iteration:
//!   1. Snapshots baseline sizes of process-global interners.
//!   2. Builds a fresh `TestApp` with the requested vim + mode.
//!   3. Translates and feeds up to `MAX_OPS` events under the default
//!      per-event allocation budget; runs `assert_invariants` after each.
//!   4. Asserts the static interners did not grow past `INTERN_SLACK`.

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use tui::app::test_harness::{AllocBudget, SourceEvent, TestApp};

use crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers,
};
use protocol::{AgentMode, EngineEvent, ToolOutcome};
use std::collections::HashMap;

#[derive(Arbitrary, Debug)]
enum FuzzOp {
    /// Single Unicode codepoint keystroke. The decoded `u32` is filtered:
    /// surrogates and out-of-range values fall back to `'?'`.
    KeyUnicode(u32),
    /// Control-modified ASCII letter.
    KeyCtrl(u8),
    /// Shift-modified printable ASCII letter.
    KeyShift(u8),
    /// Bare special key chosen by `which % SPECIALS.len()`.
    KeySpecial(u8),
    /// Bracketed paste with arbitrary UTF-8 payload.
    Paste(String),
    /// Advance the virtual clock by `ms` milliseconds.
    Tick(u16),
    /// Wake any pending Lua callbacks.
    LuaWakeup,
    /// Terminal resize. Width and height are clamped into `[1, 400]`.
    Resize { w: u16, h: u16 },

    // ── Engine-emitted events ─────────────────────────────────────────
    /// Engine `Ready` signal.
    EngineReady,
    /// Streamed assistant text (full content message).
    EngineText(String),
    /// Streaming text delta token.
    EngineTextDelta(String),
    /// Streaming thinking delta token.
    EngineThinkingDelta(String),
    /// Tool call started. `call_id` is bucketed into a small space so
    /// `EngineToolOutput`/`Finish` ops have a good chance of matching it.
    EngineToolStart {
        call_id: u8,
        tool_name: String,
    },
    /// Incremental tool stdout/stderr chunk.
    EngineToolOutput {
        call_id: u8,
        chunk: String,
    },
    /// Tool call finished. `is_error` selects success vs. failure outcome.
    EngineToolFinish {
        call_id: u8,
        is_error: bool,
        content: String,
    },
    /// Foregrounded shell-exec output line.
    ExecOutput(String),
    /// Foregrounded shell-exec done.
    ExecDone(Option<i32>),
}

#[derive(Arbitrary, Debug, Clone, Copy)]
enum FuzzMode {
    Normal,
    Plan,
    Apply,
    Yolo,
}

impl From<FuzzMode> for AgentMode {
    fn from(m: FuzzMode) -> Self {
        match m {
            FuzzMode::Normal => AgentMode::Normal,
            FuzzMode::Plan => AgentMode::Plan,
            FuzzMode::Apply => AgentMode::Apply,
            FuzzMode::Yolo => AgentMode::Yolo,
        }
    }
}

#[derive(Arbitrary, Debug)]
struct FuzzInput {
    vim: bool,
    mode: FuzzMode,
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
    KeyCode::PageUp,
    KeyCode::PageDown,
    KeyCode::Delete,
];

const MAX_OPS: usize = 256;

/// Slack allowed on top of the post-build interner baseline. Some scenarios
/// legitimately intern a few new highlight groups (picker rows, dialog
/// styles) — the leak signal is unbounded growth, not a handful of new ids.
const INTERN_SLACK: usize = 64;

/// Resize dimensions are clamped here so the fuzzer can't waste cycles on
/// pathological 65k×65k grids that won't surface real bugs.
const RESIZE_MIN: u16 = 1;
const RESIZE_MAX: u16 = 400;

fn key_event(code: KeyCode, mods: KeyModifiers) -> TermEvent {
    TermEvent::Key(KeyEvent {
        code,
        modifiers: mods,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    })
}

/// Decode an arbitrary `u32` into a real `char`, falling back to `?` for
/// surrogate halves and out-of-range values. We want broad UTF-8 coverage
/// (multi-byte, combining marks, RTL marks) without invalid Unicode.
fn decode_codepoint(raw: u32) -> char {
    char::from_u32(raw).unwrap_or('?')
}

fn clamp_dim(d: u16) -> u16 {
    d.clamp(RESIZE_MIN, RESIZE_MAX)
}

fn call_id_string(id: u8) -> String {
    format!("call-{id:02x}")
}

fn translate(op: FuzzOp) -> SourceEvent {
    match op {
        FuzzOp::KeyUnicode(raw) => {
            let c = decode_codepoint(raw);
            SourceEvent::Term(key_event(KeyCode::Char(c), KeyModifiers::NONE))
        }
        FuzzOp::KeyCtrl(b) => {
            let c = (b'a' + (b % 26)) as char;
            SourceEvent::Term(key_event(KeyCode::Char(c), KeyModifiers::CONTROL))
        }
        FuzzOp::KeyShift(b) => {
            let c = (b'a' + (b % 26)) as char;
            SourceEvent::Term(key_event(KeyCode::Char(c), KeyModifiers::SHIFT))
        }
        FuzzOp::KeySpecial(which) => {
            let code = SPECIALS[(which as usize) % SPECIALS.len()];
            SourceEvent::Term(key_event(code, KeyModifiers::NONE))
        }
        FuzzOp::Paste(s) => SourceEvent::Term(TermEvent::Paste(s)),
        FuzzOp::Tick(ms) => SourceEvent::Tick(u64::from(ms)),
        FuzzOp::LuaWakeup => SourceEvent::LuaWakeup,
        FuzzOp::Resize { w, h } => SourceEvent::Resize {
            width: clamp_dim(w),
            height: clamp_dim(h),
        },

        FuzzOp::EngineReady => SourceEvent::Engine(EngineEvent::Ready),
        FuzzOp::EngineText(s) => SourceEvent::Engine(EngineEvent::Text { content: s }),
        FuzzOp::EngineTextDelta(s) => SourceEvent::Engine(EngineEvent::TextDelta { delta: s }),
        FuzzOp::EngineThinkingDelta(s) => {
            SourceEvent::Engine(EngineEvent::ThinkingDelta { delta: s })
        }
        FuzzOp::EngineToolStart {
            call_id,
            tool_name,
        } => SourceEvent::Engine(EngineEvent::ToolStarted {
            call_id: call_id_string(call_id),
            tool_name,
            args: HashMap::new(),
        }),
        FuzzOp::EngineToolOutput { call_id, chunk } => {
            SourceEvent::Engine(EngineEvent::ToolOutput {
                call_id: call_id_string(call_id),
                chunk,
            })
        }
        FuzzOp::EngineToolFinish {
            call_id,
            is_error,
            content,
        } => SourceEvent::Engine(EngineEvent::ToolFinished {
            call_id: call_id_string(call_id),
            result: ToolOutcome {
                content,
                is_error,
                metadata: None,
            },
            elapsed_ms: Some(0),
        }),
        FuzzOp::ExecOutput(s) => SourceEvent::ExecOutput(s),
        FuzzOp::ExecDone(code) => SourceEvent::ExecDone(code),
    }
}

fuzz_target!(|input: FuzzInput| {
    let mut app = TestApp::builder()
        .with_vim(input.vim)
        .with_mode(input.mode.into())
        .build();
    let theme_baseline = smelt_style::theme::registry_len();
    let ns_baseline = smelt_buffer::buffer::namespace_count();

    let take = input.ops.len().min(MAX_OPS);
    for op in input.ops.into_iter().take(take) {
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
