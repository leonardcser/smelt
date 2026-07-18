//! Structurally-aware scenario shrinker.
//!
//! `cargo fuzz tmin` works at the byte level - it makes the libFuzzer
//! input shorter but says nothing about which ops in the decoded
//! scenario are actually load-bearing for the crash. This bin operates
//! on the **post-decode JSON**, so it can drop whole ops and shrink
//! string payloads structurally. A 200-op crash JSON typically
//! minimizes to <10 ops in a few seconds.
//!
//! Algorithm + tests live in `smelt_fuzz::shrink`. This bin is a thin
//! orchestrator: parse argv, decode JSON, wire a `catch_unwind`-based
//! predicate around the requested target, call `shrink::shrink`, write
//! the result.
//!
//! Usage:
//!
//! ```text
//! cargo run --release --bin shrink_scenario -- \
//!     [--target smelt_loop|lua_loop] <scenario.json> [out.json]
//! ```
//!
//! Default target is `smelt_loop`. If `out.json` is omitted, the
//! result is written to `<scenario>.shrunk.json`. Exits non-zero if
//! the original scenario doesn't actually panic.

use serde_json::Value;
use smelt_fuzz::shrink::{ops_count, shrink, string_chars};
use smelt_fuzz::{run_lua_scenario, run_scenario, LuaScenario, Scenario};
use std::cell::RefCell;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::{env, fs, process};

#[derive(Clone, Debug, PartialEq, Eq)]
struct PanicFingerprint {
    location: Option<(String, u32, u32)>,
    message: String,
}

thread_local! {
    static LAST_PANIC: RefCell<Option<PanicFingerprint>> = const { RefCell::new(None) };
}

#[derive(Clone, Copy)]
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
    let in_path = PathBuf::from(&positional[0]);
    let out_path = positional
        .get(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| in_path.with_extension("shrunk.json"));

    let text = fs::read_to_string(&in_path).unwrap_or_else(|e| {
        eprintln!("error: read {}: {e}", in_path.display());
        process::exit(1);
    });
    let value: Value = serde_json::from_str(&text).unwrap_or_else(|e| {
        eprintln!("error: parse JSON: {e}");
        process::exit(1);
    });

    // Capture panic identity without printing hundreds of expected panic trials.
    // The source location and normalized assertion message keep shrinking on the
    // original bug instead of accepting an unrelated panic from another path.
    let original_hook = panic::take_hook();
    panic::set_hook(Box::new(|info| {
        let location = info.location().map(|location| {
            (
                location.file().to_string(),
                location.line(),
                location.column(),
            )
        });
        let message = panic_message(info.payload());
        LAST_PANIC.with(|slot| {
            *slot.borrow_mut() = Some(PanicFingerprint {
                location,
                message: normalize_message(&message),
            });
        });
    }));

    let Some(expected) = panic_fingerprint(target, &value) else {
        panic::set_hook(original_hook);
        eprintln!(
            "error: scenario does not panic when replayed; nothing to shrink. \
             Verify with `replay_scenario {}` first.",
            in_path.display()
        );
        process::exit(1);
    };
    let same_crash =
        |candidate: &Value| panic_fingerprint(target, candidate) == Some(expected.clone());

    let before = ops_count(&value);
    let before_chars = string_chars(&value);
    let shrunk = shrink(value, same_crash);
    let after = ops_count(&shrunk);
    let after_chars = string_chars(&shrunk);

    panic::set_hook(original_hook);
    eprintln!("panic fingerprint: {expected:?}");

    let json = serde_json::to_string_pretty(&shrunk).unwrap_or_else(|e| {
        eprintln!("error: serialize: {e}");
        process::exit(1);
    });
    fs::write(&out_path, json).unwrap_or_else(|e| {
        eprintln!("error: write {}: {e}", out_path.display());
        process::exit(1);
    });

    eprintln!(
        "shrunk {}: {before} ops / {before_chars} string-chars -> {after} ops / {after_chars} string-chars",
        in_path.display(),
    );
    eprintln!("wrote {}", out_path.display());
}

fn usage_and_exit(code: i32) -> ! {
    eprintln!("usage: shrink_scenario [--target smelt_loop|lua_loop] <scenario.json> [out.json]");
    process::exit(code);
}

/// Decode and run one shrink candidate, returning the panic identity when the
/// candidate reproduces a crash. `catch_unwind` keeps trials in this process.
fn panic_fingerprint(target: Target, value: &Value) -> Option<PanicFingerprint> {
    LAST_PANIC.with(|slot| *slot.borrow_mut() = None);
    let value = value.clone();
    let result = panic::catch_unwind(AssertUnwindSafe(|| match target {
        Target::SmeltLoop => {
            let s: Scenario = match serde_json::from_value(value) {
                Ok(s) => s,
                Err(_) => return,
            };
            run_scenario(s);
        }
        Target::LuaLoop => {
            let s: LuaScenario = match serde_json::from_value(value) {
                Ok(s) => s,
                Err(_) => return,
            };
            run_lua_scenario(s);
        }
    }));
    if result.is_ok() {
        return None;
    }
    LAST_PANIC
        .with(|slot| slot.borrow_mut().take())
        .or_else(|| {
            let payload = result.expect_err("panic result");
            Some(PanicFingerprint {
                location: None,
                message: normalize_message(&panic_message(payload.as_ref())),
            })
        })
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        format!("panic payload type {:?}", payload.type_id())
    }
}

fn normalize_message(message: &str) -> String {
    let mut normalized = String::with_capacity(message.len().min(512));
    let mut in_digits = false;
    for ch in message.chars().take(512) {
        if ch.is_ascii_digit() {
            if !in_digits {
                normalized.push('#');
            }
            in_digits = true;
        } else {
            in_digits = false;
            normalized.push(ch);
        }
    }
    normalized
}
