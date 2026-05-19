//! Generic structural shrinker — delta-debugging over a JSON scenario.
//!
//! All functions take a `crashes: F` predicate where `F: Fn(&Value) ->
//! bool + Copy`. The shrinker calls `crashes` to decide whether each
//! candidate scenario still reproduces the panic. Decoupling the
//! predicate from the algorithm lets the binary supply
//! `panic::catch_unwind(run_scenario)`, and the unit tests supply a
//! cheap synthetic predicate (no fuzz target run required).
//!
//! Algorithm: classical ddmin (Zeller / Hildebrandt) on the `ops`
//! array, then per-string truncation in remaining ops, then one more
//! pass of ddmin in case the string shrinks made some ops redundant.
//!
//! Why JSON-Value-level and not typed? Both targets' scenarios are
//! `Vec<Op>` shapes that differ in `Op`'s variants. Working at the
//! serde_json::Value level lets one implementation serve both — the
//! binary picks the target only for the predicate.

use serde_json::Value;

/// Number of ops in the scenario's `ops` array (0 if missing or
/// non-array). Used by callers for before/after reporting.
pub fn ops_count(value: &Value) -> usize {
    value
        .get("ops")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0)
}

/// Total character count across every string field in the tree. Used
/// by callers for before/after reporting; cheap walk.
pub fn string_chars(value: &Value) -> usize {
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

/// Drive both passes (ops + strings) plus a closing ops pass — string
/// truncation occasionally makes a previously-load-bearing op
/// redundant.
pub fn shrink<F>(mut value: Value, crashes: F) -> Value
where
    F: Fn(&Value) -> bool + Copy,
{
    value = ddmin_ops(value, crashes);
    value = shrink_strings(value, crashes);
    ddmin_ops(value, crashes)
}

/// Delta-debugging on the `ops` array.
///
/// Pass A: drop one at a time (reverse order so removing late ops
/// doesn't shift indices we haven't tried yet) until a full sweep finds
/// no reductions. Pass B: drop power-of-two chunks descending from
/// `len/2` to 1, re-checking after every successful drop.
pub fn ddmin_ops<F>(mut value: Value, crashes: F) -> Value
where
    F: Fn(&Value) -> bool + Copy,
{
    // Pass A — drop singletons to fixed point.
    let mut changed = true;
    while changed {
        changed = false;
        let n = ops_count(&value);
        for i in (0..n).rev() {
            let cand = remove_range(&value, i, i + 1);
            if crashes(&cand) {
                value = cand;
                changed = true;
            }
        }
    }
    // Pass B — drop chunks (ddmin's group-size descent).
    let mut group = ops_count(&value).max(2) / 2;
    while group >= 1 {
        let mut i = 0;
        while i + group <= ops_count(&value) {
            let cand = remove_range(&value, i, i + group);
            if crashes(&cand) {
                value = cand;
            } else {
                i += 1;
            }
        }
        group /= 2;
    }
    value
}

/// Walk every string in `ops/*` and shrink each one independently:
/// first try empty, then halve until the truncation stops preserving
/// the crash. Idempotent under repeated invocation.
pub fn shrink_strings<F>(mut value: Value, crashes: F) -> Value
where
    F: Fn(&Value) -> bool + Copy,
{
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
            // Empty first — often a load-bearing string is load-bearing
            // by presence rather than content.
            if try_replace_string(&mut value, &path, "", crashes) {
                any_change = true;
                continue;
            }
            // Otherwise halve the prefix until it stops crashing.
            let mut len = original.chars().count();
            while len > 1 {
                let next = len / 2;
                let truncated: String = original.chars().take(next).collect();
                if try_replace_string(&mut value, &path, &truncated, crashes) {
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

fn remove_range(value: &Value, start: usize, end: usize) -> Value {
    let mut v = value.clone();
    if let Some(arr) = v.get_mut("ops").and_then(|x| x.as_array_mut()) {
        arr.drain(start..end);
    }
    v
}

fn try_replace_string<F>(value: &mut Value, ptr: &str, replacement: &str, crashes: F) -> bool
where
    F: Fn(&Value) -> bool,
{
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
    if crashes(value) {
        true
    } else {
        if let Some(slot) = value.pointer_mut(ptr) {
            *slot = Value::String(original);
        }
        false
    }
}

/// JSON pointers for every string in `ops/<i>/...`. Top-level fields
/// (`mode`, `vim`) are skipped — those drive app build options, not
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Build a scenario with `n` ops; op `i` is `{ "id": i }`.
    fn scenario_of_ids(n: usize) -> Value {
        let ops: Vec<Value> = (0..n).map(|i| json!({ "id": i })).collect();
        json!({ "vim": false, "mode": "normal", "ops": ops })
    }

    fn ids_of(v: &Value) -> Vec<u64> {
        v.get("ops")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|o| o.get("id").and_then(|x| x.as_u64()))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn ddmin_minimizes_to_required_singletons() {
        // Predicate: "crashes" iff both op id=3 and op id=7 are present.
        let crashes = |v: &Value| {
            let ids = ids_of(v);
            ids.contains(&3) && ids.contains(&7)
        };

        let initial = scenario_of_ids(20);
        assert!(crashes(&initial), "initial scenario should crash");

        let shrunk = ddmin_ops(initial, crashes);
        let ids = ids_of(&shrunk);
        assert_eq!(
            ids,
            vec![3, 7],
            "ddmin should keep only the two required ops; got {ids:?}"
        );
    }

    #[test]
    fn ddmin_minimizes_to_first_required_when_only_one_needed() {
        let crashes = |v: &Value| ids_of(v).contains(&5);
        let shrunk = ddmin_ops(scenario_of_ids(10), crashes);
        assert_eq!(ids_of(&shrunk), vec![5]);
    }

    #[test]
    fn ddmin_preserves_order_of_required_ops() {
        // Predicate: requires ids 2 and 8 in that order at least once.
        let crashes = |v: &Value| {
            let ids = ids_of(v);
            let p2 = ids.iter().position(|i| *i == 2);
            let p8 = ids.iter().position(|i| *i == 8);
            matches!((p2, p8), (Some(a), Some(b)) if a < b)
        };
        let shrunk = ddmin_ops(scenario_of_ids(15), crashes);
        assert_eq!(ids_of(&shrunk), vec![2, 8]);
    }

    #[test]
    fn shrink_strings_truncates_unused_payloads() {
        let predicate_ids: &[u64] = &[1, 4];
        let scenario = json!({
            "vim": false,
            "mode": "normal",
            "ops": [
                { "id": 0, "payload": "aaaaaaaaaa" },
                { "id": 1, "payload": "bbbbbbbbbb" },
                { "id": 2, "payload": "cccccccccc" },
                { "id": 4, "payload": "dddddddddd" }
            ]
        });
        // Crash depends only on which ids are present — not on
        // payload contents.
        let crashes = |v: &Value| {
            let ids = ids_of(v);
            predicate_ids.iter().all(|i| ids.contains(i))
        };
        assert!(crashes(&scenario));

        let shrunk = shrink_strings(scenario, crashes);
        // Every remaining payload string should shrink to "" since
        // the predicate ignores content.
        for op in shrunk
            .get("ops")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            let p = op
                .get("payload")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            assert_eq!(p, "", "expected payload shrunk to empty, got {p:?}");
        }
    }

    #[test]
    fn shrink_strings_preserves_load_bearing_substring() {
        // Crash iff op with id=0 has a payload that *starts with* "ab".
        let crashes = |v: &Value| {
            let ops = v.get("ops").and_then(|x| x.as_array());
            let Some(ops) = ops else { return false };
            ops.iter().any(|op| {
                op.get("id").and_then(|x| x.as_u64()) == Some(0)
                    && op
                        .get("payload")
                        .and_then(|x| x.as_str())
                        .is_some_and(|s| s.starts_with("ab"))
            })
        };
        let scenario = json!({
            "vim": false,
            "mode": "normal",
            "ops": [{ "id": 0, "payload": "abcdefghij" }]
        });
        assert!(crashes(&scenario));
        let shrunk = shrink_strings(scenario, crashes);
        let payload = shrunk
            .pointer("/ops/0/payload")
            .and_then(|v| v.as_str())
            .unwrap();
        // Must still start with "ab"; halving prefix preserves that.
        assert!(
            payload.starts_with("ab"),
            "shrunk payload {payload:?} dropped the load-bearing prefix"
        );
    }

    #[test]
    fn full_shrink_combines_ops_and_strings() {
        let crashes = |v: &Value| {
            let ops = v.get("ops").and_then(|x| x.as_array());
            let Some(ops) = ops else { return false };
            ops.iter().any(|op| {
                op.get("payload").and_then(|x| x.as_str()) == Some("KEY")
            })
        };
        let scenario = json!({
            "vim": false,
            "mode": "normal",
            "ops": [
                { "id": 0, "payload": "noise-a" },
                { "id": 1, "payload": "noise-b" },
                { "id": 2, "payload": "KEY" },
                { "id": 3, "payload": "noise-c" },
                { "id": 4, "payload": "noise-d" }
            ]
        });
        let shrunk = shrink(scenario, crashes);
        let ops = shrunk.get("ops").and_then(|v| v.as_array()).unwrap();
        // Only one op remains.
        assert_eq!(ops.len(), 1, "expected exactly one op after full shrink");
        // And its payload is the load-bearing literal.
        assert_eq!(
            ops[0].get("payload").and_then(|v| v.as_str()),
            Some("KEY")
        );
    }
}
