//! Replay a `Scenario` JSON file against a fresh `TestApp`.
//!
//! Usage:
//!
//! ```text
//! cargo run --bin replay_scenario -- [--trace] <scenario.json>
//! ```
//!
//! With `--trace`, prints prompt + transcript window state after each op so
//! invariant violations can be located precisely. Without it, just runs the
//! scenario and exits non-zero on any panic or assertion.

use smelt_fuzz::Scenario;
use std::path::Path;
use std::{env, fs, process};

fn main() {
    let args: Vec<String> = env::args().collect();
    let trace = args.iter().any(|a| a == "--trace");
    let path_arg = args.iter().skip(1).find(|a| !a.starts_with("--"));
    let Some(path_arg) = path_arg else {
        eprintln!("usage: replay_scenario [--trace] <scenario.json>");
        process::exit(2);
    };
    let path = Path::new(path_arg);
    let text = fs::read_to_string(path).expect("read scenario");
    let scenario: Scenario = serde_json::from_str(&text).expect("parse scenario");

    if trace {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _g = runtime.enter();
        let mut app = tui::app::test_harness::TestApp::builder()
            .with_vim(scenario.vim)
            .with_mode(scenario.mode.into())
            .build();
        let take = scenario.ops.len().min(smelt_fuzz::MAX_OPS);
        for (i, op) in scenario.ops.into_iter().take(take).enumerate() {
            eprintln!("--- op{i} {op:?} ---");
            smelt_fuzz::apply(&mut app, op);
            for wid in [
                tui::app::PROMPT_WIN,
                tui::app::TRANSCRIPT_WIN,
                tui::app::PROMPT_ABOVE_WIN,
                tui::app::PROMPT_BELOW_WIN,
            ] {
                let win = match app.app.ui.win(wid) {
                    Some(w) => w,
                    None => continue,
                };
                let buf = app.app.ui.buf(win.buf);
                let slen = buf.map(|b| b.source().len()).unwrap_or(0);
                eprintln!(
                    "  {:?} cpos={} src.len={} vim_mode={:?} sel_anchor={:?}",
                    wid, win.cpos, slen, win.vim_mode, win.selection_anchor
                );
            }
            if app.quit_requested() {
                break;
            }
        }
        eprintln!("ok: {}", path.display());
        return;
    }

    smelt_fuzz::run_scenario(scenario);
    eprintln!("ok: {}", path.display());
}
