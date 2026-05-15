//! Replay a `Scenario` JSON file against a fresh `TestApp`.
//!
//! Usage:
//!
//! ```text
//! cargo run --bin replay_scenario -- <scenario.json>
//! ```
//!
//! Exits non-zero on any invariant violation, allocation-budget breach,
//! or panic inside the harness. Intended as the substrate for CI
//! regression checks (commit `tests/regressions/*.json`, run each) and
//! for the eventual terminal-replay UI.

use smelt_fuzz::Scenario;
use std::path::Path;
use std::{env, fs, process};

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        eprintln!("usage: replay_scenario <scenario.json>");
        process::exit(2);
    }
    let path = Path::new(&args[1]);
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: read {}: {e}", path.display());
            process::exit(1);
        }
    };
    let scenario: Scenario = match serde_json::from_str(&text) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: parse {}: {e}", path.display());
            process::exit(1);
        }
    };
    smelt_fuzz::run_scenario(scenario);
    eprintln!("ok: {}", path.display());
}
