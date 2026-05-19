//! Structurally-aware scenario shrinker.
//!
//! `cargo fuzz tmin` works at the byte level — it makes the libFuzzer
//! input shorter but says nothing about which ops in the decoded
//! scenario are actually load-bearing for the crash. This bin operates
//! on the **post-decode JSON**, so it can drop whole ops and shrink
//! string payloads structurally. A 200-op crash JSON typically minimizes
//! to <10 ops in a few seconds.
//!
//! Usage:
//!
//! ```text
//! cargo run --release --bin shrink_scenario -- \
//!     [--target smelt_loop|lua_loop] <scenario.json> [out.json]
//! ```
//!
//! Default target is `smelt_loop`. If `out.json` is omitted, the
//! result is written to `<scenario>.shrunk.json`.
//!
//! Crash preservation: any panic from the run counts. If the original
//! scenario doesn't panic, the bin exits non-zero immediately.

use serde_json::Value;
use smelt_fuzz::{run_lua_scenario, run_scenario, LuaScenario, Scenario};
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::{env, fs, process};

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

    // Suppress panic output during shrinking — we expect ~hundreds of
    // panics per run. Restore the original hook before the final
    // verification so the user still sees the crash they care about.
    let original_hook = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    if !crashes(target, &value) {
        panic::set_hook(original_hook);
        eprintln!(
            "error: scenario does not panic when replayed; nothing to shrink. \
             Verify with `replay_scenario {}` first.",
            in_path.display()
        );
        process::exit(1);
    }

    let before = ops_count(&value);
    let before_chars = string_chars(&value);
    let shrunk = shrink(target, value);
    let after = ops_count(&shrunk);
    let after_chars = string_chars(&shrunk);

    panic::set_hook(original_hook);

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
    eprintln!(
        "usage: shrink_scenario [--target smelt_loop|lua_loop] <scenario.json> [out.json]"
    );
    process::exit(code);
}

fn ops_count(value: &Value) -> usize {
    value
        .get("ops")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0)
}

fn string_chars(value: &Value) -> usize {
    fn walk(v: &Value, acc: &mut usize) {
        match v {
            Value::String(s) => *acc += s.chars().count(),
            Value::Array(a) => a.iter().for_each(|c| walk(c, acc)),
            Value::Object(o) => o.values().for_each(|c| walk(c, acc)),
            _ => {}
        }
    }
    let mut acc = 0;
    walk(value, &mut acc);
    acc
}

/// Decode + run. Returns `true` if the run panics. Uses `catch_unwind`
/// so individual shrink trials don't abort the whole bin.
fn crashes(target: Target, value: &Value) -> bool {
    let value = value.clone();
    panic::catch_unwind(AssertUnwindSafe(|| match target {
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
    }))
    .is_err()
}

fn shrink(target: Target, mut value: Value) -> Value {
    value = ddmin_ops(target, value);
    value = shrink_strings(target, value);
    // One more ops pass after string shrinks — strings sometimes make a
    // previously-load-bearing op redundant.
    ddmin_ops(target, value)
}

/// Delta-debugging on the `ops` array: drop singles to fixed point,
/// then drop power-of-two chunks descending from `len/2` to 1.
fn ddmin_ops(target: Target, mut value: Value) -> Value {
    // Pass A: drop one at a time, reverse so removing late ops doesn't
    // shift indices we haven't tried yet.
    let mut changed = true;
    while changed {
        changed = false;
        let n = ops_count(&value);
        for i in (0..n).rev() {
            let cand = remove_range(&value, i, i + 1);
            if crashes(target, &cand) {
                value = cand;
                changed = true;
            }
        }
    }
    // Pass B: drop chunks (ddmin's group-size descent).
    let mut group = ops_count(&value).max(2) / 2;
    while group >= 1 {
        let mut i = 0;
        while i + group <= ops_count(&value) {
            let cand = remove_range(&value, i, i + group);
            if crashes(target, &cand) {
                value = cand;
            } else {
                i += 1;
            }
        }
        group /= 2;
    }
    value
}

fn remove_range(value: &Value, start: usize, end: usize) -> Value {
    let mut v = value.clone();
    if let Some(arr) = v.get_mut("ops").and_then(|x| x.as_array_mut()) {
        arr.drain(start..end);
    }
    v
}

/// Walk every string in the tree and try truncating it. Each string is
/// shrunk independently to a fixed point: empty first, then prefix
/// halves until the truncation no longer preserves the crash. Visits
/// strings by `(op_idx, json_pointer)` so trial substitutions can be
/// applied in isolation.
fn shrink_strings(target: Target, mut value: Value) -> Value {
    // Collect (path, original_len). Iterate by index so we can mutate
    // through fresh `pointer_mut` lookups after each successful shrink.
    loop {
        let paths = string_paths(&value);
        if paths.is_empty() {
            return value;
        }
        let mut any_change = false;
        for path in paths {
            let original = match value.pointer(&path).and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => continue,
            };
            // Try empty first — usually load-bearing strings still
            // crash on empty if their *presence* (not content) drives
            // the bug.
            if try_replace_string(&mut value, &path, "", target) {
                any_change = true;
                continue;
            }
            // Otherwise halve the prefix until it stops crashing.
            let mut len = original.chars().count();
            while len > 1 {
                let next = len / 2;
                let truncated: String = original.chars().take(next).collect();
                if try_replace_string(&mut value, &path, &truncated, target) {
                    len = next;
                    any_change = true;
                } else {
                    break;
                }
            }
        }
        if !any_change {
            return value;
        }
    }
}

fn try_replace_string(value: &mut Value, ptr: &str, replacement: &str, target: Target) -> bool {
    let original = value
        .pointer(ptr)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let Some(original) = original else {
        return false;
    };
    if let Some(slot) = value.pointer_mut(ptr) {
        *slot = Value::String(replacement.to_string());
    }
    if crashes(target, value) {
        true
    } else {
        if let Some(slot) = value.pointer_mut(ptr) {
            *slot = Value::String(original);
        }
        false
    }
}

/// JSON pointers for every string in `ops/<i>/...`. Other top-level
/// keys (mode, vim) are skipped — their value affects build_app, not
/// op payloads, and shrinking them rarely helps.
fn string_paths(value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(arr) = value.get("ops").and_then(|v| v.as_array()) else {
        return out;
    };
    for (i, op) in arr.iter().enumerate() {
        collect_strings(op, &format!("/ops/{i}"), &mut out);
    }
    out
}

fn collect_strings(v: &Value, prefix: &str, out: &mut Vec<String>) {
    match v {
        Value::String(_) => out.push(prefix.to_string()),
        Value::Array(a) => {
            for (i, child) in a.iter().enumerate() {
                collect_strings(child, &format!("{prefix}/{i}"), out);
            }
        }
        Value::Object(o) => {
            for (k, child) in o.iter() {
                // JSON-pointer escape rules: `~` -> `~0`, `/` -> `~1`.
                let escaped = k.replace('~', "~0").replace('/', "~1");
                collect_strings(child, &format!("{prefix}/{escaped}"), out);
            }
        }
        _ => {}
    }
}

