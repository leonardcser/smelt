//! Replay a scenario JSON file against a fresh `TestApp`.
//!
//! Usage:
//!
//! ```text
//! cargo run --features scenario-tools --bin replay_scenario -- \
//!     [--target smelt_loop|lua_loop] [--trace] <scenario.json>
//! ```
//!
//! Default target is `smelt_loop`. With `--trace` (smelt_loop only),
//! prints prompt + transcript window state after each op so invariant
//! violations can be located precisely. Without it, just runs the
//! scenario and exits non-zero on any panic or assertion.

use smelt_fuzz::{LuaScenario, Scenario};
use std::path::Path;
use std::{env, fs, process};

enum Target {
    SmeltLoop,
    LuaLoop,
}

impl Target {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "smelt_loop" => Some(Target::SmeltLoop),
            "lua_loop" => Some(Target::LuaLoop),
            _ => None,
        }
    }
}

fn main() {
    let argv: Vec<String> = env::args().collect();
    let mut target = Target::SmeltLoop;
    let mut trace = false;
    let mut positional: Vec<String> = Vec::new();
    let mut i = 1;
    while i < argv.len() {
        match argv[i].as_str() {
            "--target" => {
                i += 1;
                let Some(arg) = argv.get(i) else {
                    usage_and_exit(2);
                };
                let Some(t) = Target::parse(arg) else {
                    eprintln!("error: unknown --target {arg:?}");
                    process::exit(2);
                };
                target = t;
            }
            "--trace" => trace = true,
            "-h" | "--help" => usage_and_exit(0),
            other if other.starts_with("--") => {
                eprintln!("error: unknown flag {other:?}");
                process::exit(2);
            }
            _ => positional.push(argv[i].clone()),
        }
        i += 1;
    }
    if positional.len() != 1 {
        usage_and_exit(2);
    }
    let path = Path::new(&positional[0]);
    let text = fs::read_to_string(path).expect("read scenario");

    match target {
        Target::SmeltLoop => replay_smelt(&text, path, trace),
        Target::LuaLoop => replay_lua(&text, path, trace),
    }
}

fn replay_smelt(text: &str, path: &Path, trace: bool) {
    let scenario: Scenario = serde_json::from_str(text).expect("parse scenario");

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
            for wid in [tui::app::PROMPT_WIN, tui::app::TRANSCRIPT_WIN] {
                let win = match app.app.ui.win(wid) {
                    Some(w) => w,
                    None => continue,
                };
                let buf = app.app.ui.buf(win.buf);
                let slen = buf.map(|b| b.source().len()).unwrap_or(0);
                eprintln!(
                    "  {:?} cpos={} src.len={} vim_mode={:?} sel_anchor={:?}",
                    wid,
                    win.cpos(),
                    slen,
                    win.vim_mode(),
                    win.selection_anchor()
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

fn replay_lua(text: &str, path: &Path, trace: bool) {
    if trace {
        eprintln!(
            "warning: --trace is not supported for lua_loop scenarios (no per-op decomposition); ignoring"
        );
    }
    let scenario: LuaScenario = serde_json::from_str(text).expect("parse scenario");
    smelt_fuzz::run_lua_scenario(scenario);
    eprintln!("ok: {}", path.display());
}

fn usage_and_exit(code: i32) -> ! {
    eprintln!("usage: replay_scenario [--target smelt_loop|lua_loop] [--trace] <scenario.json>");
    process::exit(code);
}
