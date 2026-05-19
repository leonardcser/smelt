//! Convert a libFuzzer crash artifact (raw bytes) into a JSON scenario.
//!
//! Usage:
//!
//! ```text
//! cargo run --bin crash_to_scenario -- \
//!     [--target smelt_loop|lua_loop] <artifact-path> [out.json]
//! ```
//!
//! Default target is `smelt_loop`. If `out.json` is omitted, the
//! scenario is printed to stdout. The same `Arbitrary` decoder
//! libFuzzer uses inside `fuzz_target!` runs here, so the resulting
//! scenario is exactly the input shape that produced the original
//! crash.

use arbitrary::{Arbitrary, Unstructured};
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
            "-h" | "--help" => usage_and_exit(0),
            other if other.starts_with("--") => {
                eprintln!("error: unknown flag {other:?}");
                process::exit(2);
            }
            _ => positional.push(argv[i].clone()),
        }
        i += 1;
    }
    if positional.is_empty() || positional.len() > 2 {
        usage_and_exit(2);
    }
    let artifact = Path::new(&positional[0]);
    let bytes = fs::read(artifact).unwrap_or_else(|e| {
        eprintln!("error: read {}: {e}", artifact.display());
        process::exit(1);
    });

    // libFuzzer feeds `arbitrary_take_rest` for the full byte budget; mirror
    // that here so artifacts decode identically to the fuzz iteration that
    // produced them.
    let u = Unstructured::new(&bytes);
    let json = match target {
        Target::SmeltLoop => {
            let scenario = Scenario::arbitrary_take_rest(u).unwrap_or_else(|e| {
                eprintln!("error: decode {}: {e}", artifact.display());
                process::exit(1);
            });
            serde_json::to_string_pretty(&scenario)
        }
        Target::LuaLoop => {
            let scenario = LuaScenario::arbitrary_take_rest(u).unwrap_or_else(|e| {
                eprintln!("error: decode {}: {e}", artifact.display());
                process::exit(1);
            });
            serde_json::to_string_pretty(&scenario)
        }
    }
    .unwrap_or_else(|e| {
        eprintln!("error: serialize: {e}");
        process::exit(1);
    });

    if let Some(out_arg) = positional.get(1) {
        let out = Path::new(out_arg);
        fs::write(out, &json).unwrap_or_else(|e| {
            eprintln!("error: write {}: {e}", out.display());
            process::exit(1);
        });
        eprintln!("wrote {}", out.display());
    } else {
        println!("{json}");
    }
}

fn usage_and_exit(code: i32) -> ! {
    eprintln!("usage: crash_to_scenario [--target smelt_loop|lua_loop] <artifact-path> [out.json]");
    process::exit(code);
}
