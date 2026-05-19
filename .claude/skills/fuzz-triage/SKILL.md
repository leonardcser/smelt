---
name: fuzz-triage
description: This skill should be used when the user asks to "start fuzzing", "run the fuzzers", "fuzz the repo", "continue fuzz testing", or similar requests to drive the smelt fuzz suite in fix-as-you-go mode. Covers launching the two cargo-fuzz targets in background and the one-cycle-per-crash triage loop.
version: 1.0.0
---

# Fuzz triage loop (fix-as-you-go)

Drive the `smelt_loop` and `text_ops` cargo-fuzz targets in parallel under
`-ignore_crashes=0` so libFuzzer stops on the first crash, then triage and
fix in a tight cycle before relaunching.

## When to use

- "start fuzzing", "run the fuzzers", "fuzz testing", "continue fuzzing"
- After committing a fix, when the user asks to relaunch a single target
- Any request that maps to running cargo-fuzz on the smelt fuzz crate

## Launch (both targets, background)

Run from the fuzzing worktree's root (the cwd you're already in — don't
hardcode the path).

```bash
# smelt_loop gets ~half the cores (it's the CPU-heavy harness target).
# Clamp to at least 1.
LOOP_FORK=$(( $(nproc) / 2 )); [ "$LOOP_FORK" -lt 1 ] && LOOP_FORK=1
cargo +nightly fuzz run --sanitizer=none smelt_loop -- -fork="$LOOP_FORK" -ignore_crashes=0

# text_ops is much faster per iteration; 4 workers saturate it.
cargo +nightly fuzz run --sanitizer=none text_ops -- -fork=4 -ignore_crashes=0
```

Both invocations are background tasks (each gets its own task id). Do NOT
poll, do NOT schedule periodic wakeups — libFuzzer exits non-zero on the
first crash and the harness fires a background-task failure notification.
Silence = healthy.

## On crash notification

Triage one cycle per crash. The other target keeps running throughout.

1. **Locate**: read the failing task output (it includes the artifact path,
   debug-decoded `FuzzInput`, and the panic message). Newest artifact is in
   `fuzz/artifacts/<target>/`.

2. **Minimize** (byte-level, then structural):
   ```bash
   cargo +nightly fuzz tmin --sanitizer=none <target> <artifact>
   ```

3. **Decode + structural shrink + trace** (the bins live in `fuzz/`):
   ```bash
   cd fuzz && cargo build --release --bin replay_scenario --bin crash_to_scenario --bin shrink_scenario
   # Replace `--target smelt_loop` with `--target lua_loop` for lua artifacts.
   ./target/release/crash_to_scenario <minimized-artifact> /tmp/raw.json
   ./target/release/shrink_scenario /tmp/raw.json /tmp/min.json
   ./target/release/replay_scenario --trace /tmp/min.json
   ```
   The structural shrinker drops whole ops + halves string payloads, so a
   200-op tmin output typically lands at <10 ops. `--trace` prints per-op
   `(cpos, src.len, vim_mode, sel_anchor)` for every window — usually
   enough to identify which op flipped the invariant.

4. **Root cause first, fix once**: panic prefixes (`INV-NN`, `AttachedTextMut`,
   etc.) map to specific invariants. Form a single hypothesis before
   touching code. Do not widen tolerance, suppress with `#[allow]`, or
   silently re-snap. UTF-8 boundary / attachment-marker rules are in
   `AGENTS.md` under "UTF-8 safety".

5. **Verify in this order**:
   ```bash
   cargo fmt && cargo clippy --workspace --all-targets -- -D warnings
   cargo nextest run --workspace
   ```
   Then rebuild the replay binary and **replay every artifact** under
   `fuzz/artifacts/`, not just the new one:
   ```bash
   cd fuzz && cargo build --release --bin replay_scenario --bin crash_to_scenario
   for f in artifacts/smelt_loop/crash-* artifacts/smelt_loop/minimized-*; do
     [ -f "$f" ] || continue
     name=$(basename "$f")
     out=/tmp/triage/$name.json
     mkdir -p /tmp/triage
     ./target/release/crash_to_scenario "$f" "$out" > /dev/null 2>&1
     result=$(./target/release/replay_scenario "$out" 2>&1)
     if echo "$result" | grep -q "^ok"; then
       echo "  PASS $name"
     else
       echo "  FAIL $name"
       echo "$result" | tail -3
     fi
   done
   ```
   Any FAIL → back to step 4.

6. **Commit** (conventional commits, single subject line, no body, no
   co-author).

7. **Relaunch only the crashed target** with the same command from "Launch".
   Do not auto-relaunch on every notification — only after a fix is committed.

## Constraints

- No remote push, no `--force`, no rebase onto remote.
- Don't reference internal phase plans in commit messages.
