//! Shared types for the smelt fuzz target and the `crash_to_scenario`
//! converter. The on-disk scenario format is a JSON-serialized
//! [`Scenario`] - also the exact shape libFuzzer's `arbitrary` decoder
//! produces, so a crash artifact round-trips into a readable file with no
//! lossy translation.

pub mod arb;
pub mod cache_common;
pub mod engine_sim;
pub mod lua_loop;
pub mod runtime;
pub mod shrink;
pub use lua_loop::{run_lua_scenario, LuaScenario};
mod op;
mod oracle;
mod plan;
mod scenario;
pub use op::*;
use oracle::*;
pub use plan::apply;
pub use scenario::*;

use arb::short_string;
use arbitrary::{Arbitrary, Unstructured};
use crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton,
    MouseEvent, MouseEventKind,
};
use protocol::UiCommand;
use protocol::{
    AgentMode, CanonicalHistoryDelta, Content, EngineAskError, EngineAskErrorKind, EngineEvent,
    Message, ReasoningBlock, ReasoningKind, TokenUsage, ToolOutcome,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tui::app::test_harness::{Action, AllocBudget, SourceEvent};
use tui::smelt_edit::VimMode;

pub use tui::app::test_harness::TestApp;
use tui::app::test_harness::TestAppBuilder;

/// Builder for every UI-level fuzz target. Sessions are ephemeral so the
/// writer is claimed synchronously: a persistent writer reports status from a
/// background thread, which makes the same input pass or fail by timing.
/// Persistence has its own deterministic target, `store_state`.
pub fn app_builder() -> TestAppBuilder {
    TestApp::builder().with_ephemeral(true)
}

pub const SPECIALS: &[KeyCode] = &[
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

pub const MAX_OPS: usize = 256;

/// Slack allowed on top of the post-build interner baseline.
pub const INTERN_SLACK: usize = 64;

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

fn decode_codepoint(raw: u32) -> char {
    char::from_u32(raw).unwrap_or('?')
}

fn clamp_dim(d: u16) -> u16 {
    d.clamp(RESIZE_MIN, RESIZE_MAX)
}

/// Curated `(accept, dismiss)` chord pairs for `InstallPlaceholder`.
/// Each variant exercises a different placeholder dispatch path: bare
/// keys, modifier chords, multi-chord accept sets, and the empty-set
/// case where the placeholder is purely cosmetic.
fn placeholder_chord_pair(
    variant: u8,
) -> (Vec<tui::smelt_edit::KeyBind>, Vec<tui::smelt_edit::KeyBind>) {
    use tui::smelt_edit::KeyBind;
    let kb = |code, mods| KeyBind { code, mods };
    let tab = kb(KeyCode::Tab, KeyModifiers::NONE);
    let enter = kb(KeyCode::Enter, KeyModifiers::NONE);
    let esc = kb(KeyCode::Esc, KeyModifiers::NONE);
    let right = kb(KeyCode::Right, KeyModifiers::NONE);
    let ctrl_n = kb(KeyCode::Char('n'), KeyModifiers::CONTROL);
    let ctrl_g = kb(KeyCode::Char('g'), KeyModifiers::CONTROL);
    let ctrl_y = kb(KeyCode::Char('y'), KeyModifiers::CONTROL);
    match variant % 6 {
        0 => (vec![tab], vec![esc]),
        1 => (vec![enter], vec![esc]),
        2 => (vec![ctrl_n], vec![ctrl_g]),
        3 => (vec![tab, right, ctrl_y], vec![esc, ctrl_g]),
        4 => (vec![], vec![esc]),
        _ => (vec![], vec![]),
    }
}

/// Compress the random `u8` call-id space down to `CALL_ID_BUCKETS` so
/// `ToolStarted` and `ToolFinished` actually pair up under random fuzz
/// inputs. Without this, collisions happen at 1/256 per pair, which the
/// coverage report shows is too rare to ever exercise the matching
/// branches in `handle_engine_event`.
const CALL_ID_BUCKETS: u8 = 8;

fn fuzz_invocation_id(id: u8) -> protocol::InvocationId {
    protocol::InvocationId::new(u64::from(id % CALL_ID_BUCKETS))
}

fn call_id_string(id: u8) -> String {
    format!("call-{:02x}", id % CALL_ID_BUCKETS)
}

/// Synthesize a deterministic, lightweight history vector for compaction
/// payloads. Rotates through user, terminal assistant, and tool-call assistant
/// turns so history updates and screen restoration exercise every variant.
fn synth_history(count: usize) -> Vec<protocol::HistoryItem> {
    (0..count)
        .map(|i| {
            let body = format!("compacted-{i}");
            let reasoning_blocks = if i % 4 == 0 {
                Vec::new()
            } else {
                vec![ReasoningBlock {
                    provider: "fuzz".to_string(),
                    data: serde_json::Value::Null,
                }]
            };
            match i % 3 {
                0 => protocol::HistoryItem::user(Content::text(body)),
                1 => protocol::HistoryItem::Assistant(protocol::AssistantStep::terminal(
                    Some(Content::text(body)),
                    None,
                    reasoning_blocks,
                )),
                _ => {
                    let invocation = protocol::ToolInvocation {
                        call_id: format!("synth-call-{i:02}"),
                        name: "synth".to_string(),
                        arguments: "{}".to_string(),
                        result: ToolOutcome::new(format!("synth-result-{i}"), false, None),
                        elapsed_ms: None,
                        called_at_ms: Some(i as u64),
                        provider: None,
                    };
                    protocol::HistoryItem::Assistant(protocol::AssistantStep::with_invocations(
                        Some(Content::text(body)),
                        None,
                        reasoning_blocks,
                        vec![invocation],
                    ))
                }
            }
        })
        .collect()
}

/// Should `run_scenario` render a frame after applying this op? `true`
/// covers anything that changes what would be on screen: input edits,
/// engine stream updates, resize, mouse drag, overlay/dialog motion,
/// reload (re-fires `on_ready`, may open splashes). `false` for ops
/// that only nudge clock or queue-meta state (`Tick`, `EngineReady`,
/// `EngineTokenUsage`) - rendering after those is pure cost. Lifting
/// every `content/*` parser out of 0% coverage is exactly what this
/// classification unlocks; the trade-off is one frame projection per
/// non-trivial op (≈50% of ops, on average).
fn render_trigger(op: &FuzzOp) -> bool {
    !matches!(
        op,
        FuzzOp::Tick(_) | FuzzOp::EngineReady | FuzzOp::EngineTokenUsage { .. }
    )
}

/// Build a fresh `TestApp` configured for the scenario's initial state.
/// Bypasses the invariant-only path so visual replay code can advance
/// step-by-step.
pub fn build_app(scenario: &Scenario) -> TestApp {
    app_builder()
        .with_vim(scenario.vim)
        .with_mode(scenario.mode.into())
        .build()
}

/// Apply the first `n` ops from `scenario` to `app`. Used by replay
/// drivers that need to rewind to an earlier step by rebuilding and
/// fast-forwarding.
pub fn apply_n(app: &mut TestApp, scenario: &Scenario, n: usize) {
    let n = n.min(scenario.ops.len()).min(MAX_OPS);
    for op in scenario.ops.iter().take(n).cloned() {
        apply(app, op);
        if app.quit_requested() {
            break;
        }
    }
}

/// Drive a fresh `TestApp` through a scenario from start to finish.
/// Returns when the scenario is exhausted or the app requests quit.
/// Used by the fuzz target itself and by any external replay code that
/// just wants to re-run a scenario to confirm a crash.
pub fn run_scenario(scenario: Scenario) {
    run_scenario_observed(scenario, |_, _| {});
}

/// Replay with the same runtime isolation, render scheduling, and invariants as
/// fuzzing. Observers can inspect each applied operation without a second driver.
pub fn run_scenario_observed(scenario: Scenario, mut observe: impl FnMut(usize, &TestApp)) {
    runtime::with_current_thread_runtime("fuzz harness", || {
        let mut app = build_app(&scenario);
        let theme_baseline = smelt_style::theme::registry_len();
        let ns_baseline = smelt_buffer::buffer::namespace_count();

        let take = scenario.ops.len().min(MAX_OPS);
        for (index, op) in scenario.ops.into_iter().take(take).enumerate() {
            let render_after = render_trigger(&op);
            apply(&mut app, op);
            observe(index, &app);
            if app.quit_requested() {
                break;
            }
            if render_after {
                app.render_silent();
            }
        }
        // Always render once at the end so the final state passes through the
        // projection - covers scenarios that end on a `Tick` and would
        // otherwise skip the renderer entirely.
        app.render_silent();

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
        app.assert_lua_runtime_released();
    });
}
