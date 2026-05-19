---
name: fuzz-triage
description: This skill should be used when the user asks to "start fuzzing", "run the fuzzers", "fuzz the repo", "continue fuzz testing", or similar requests to drive the smelt fuzz suite in fix-as-you-go mode. Covers launching every cargo-fuzz target in parallel background processes and the one-cycle-per-crash triage loop.
version: 2.0.0
---

# Fuzz triage loop (fix-as-you-go)

Drive every smelt fuzz target in parallel under `-ignore_crashes=0` so
libFuzzer stops on the first crash, then triage and fix in a tight cycle
before relaunching.

## When to use

- "start fuzzing", "run the fuzzers", "fuzz testing", "continue fuzzing"
- After committing a fix, when the user asks to relaunch a target
- Any request that maps to running cargo-fuzz on the smelt fuzz crate

## Targets

Seven targets, two flavors:

| target | flavor | weight |
|---|---|---|
| `smelt_loop` | TUI harness — CPU-heavy | heavy |
| `lua_loop` | mlua FFI harness — CPU-heavy | heavy |
| `attached_ops` | bytes — fast | medium |
| `cache_invariance` | bytes — fast | light |
| `openai_cache_invariance` | bytes — fast | light |
| `snapshot_roundtrip` | bytes — fast | light |
| `text_ops` | bytes — fastest | light |

## Launch (all targets, background, parallel)

The user specifies a total fork budget. Distribute across the seven targets
weighting the heavy ones higher. Default split for **20 forks**:

| target | forks |
|---|---|
| `smelt_loop` | 6 |
| `lua_loop` | 6 |
| `attached_ops` | 3 |
| `cache_invariance` | 1 |
| `openai_cache_invariance` | 2 |
| `snapshot_roundtrip` | 1 |
| `text_ops` | 1 |

For a different budget, scale proportionally. Heavy targets get the
biggest share because their per-iteration cost dominates; the byte
targets saturate at low fork counts.

Launch each target with `run_in_background: true`, redirecting stdout +
stderr to `/tmp/fuzz-loop/fuzz-<target>.log`. Use the xtask wrapper —
it sets `-ignore_crashes=0` and validates the target name:

```bash
mkdir -p /tmp/fuzz-loop
cargo xtask fuzz run smelt_loop --fork 6              > /tmp/fuzz-loop/fuzz-smelt_loop.log 2>&1
cargo xtask fuzz run lua_loop --fork 6                > /tmp/fuzz-loop/fuzz-lua_loop.log 2>&1
cargo xtask fuzz run attached_ops --fork 3            > /tmp/fuzz-loop/fuzz-attached_ops.log 2>&1
cargo xtask fuzz run cache_invariance --fork 1        > /tmp/fuzz-loop/fuzz-cache_invariance.log 2>&1
cargo xtask fuzz run openai_cache_invariance --fork 2 > /tmp/fuzz-loop/fuzz-openai_cache_invariance.log 2>&1
cargo xtask fuzz run snapshot_roundtrip --fork 1      > /tmp/fuzz-loop/fuzz-snapshot_roundtrip.log 2>&1
cargo xtask fuzz run text_ops --fork 1                > /tmp/fuzz-loop/fuzz-text_ops.log 2>&1
```

**Do NOT poll, do NOT schedule wakeups.** Each background task fires a
notification the moment it exits. `cargo xtask fuzz run` only exits on
crash or Ctrl-C, so silence = healthy. The notification *is* the loop
signal.

## On crash notification

Triage one cycle per crash. The other targets keep running throughout.

1. **Locate.** Read the failing task output for the artifact path and
   panic message. Newest artifact also lives in `fuzz/artifacts/<target>/`.

2. **Triage (JSON-scenario targets: `smelt_loop`, `lua_loop`).**

   ```bash
   cargo xtask fuzz triage <target> fuzz/artifacts/<target>/crash-<hex>
   ```

   This builds the in-tree tools, runs `crash_to_scenario` →
   `shrink_scenario`, prints the minimal scenario JSON, and tells you
   where to drop it as a regression seed. Typical 200-op artifact shrinks
   to under 10 ops.

3. **Triage (byte targets: `text_ops`, `attached_ops`, `cache_invariance`,
   `openai_cache_invariance`).** No scenario form; minimize bytes
   directly:

   ```bash
   cargo +nightly fuzz tmin --sanitizer=none <target> <artifact>
   ```

   The minimized artifact lands next to the original. Read it back with
   `cargo +nightly fuzz run --sanitizer=none <target> <minimized> -- -runs=1`
   to confirm it still panics.

4. **Root cause first, fix once.** Panic prefixes (`INV-NN`,
   `AttachedTextMut`, `CACHE:`, etc.) map to specific invariants. Form a
   single hypothesis before touching code. Do not widen tolerance,
   suppress with `#[allow]`, or silently re-snap. UTF-8 boundary /
   attachment-marker rules are in `AGENTS.md` under "UTF-8 safety";
   prompt-cache rules are next to the `cache_*` provider tests.

5. **Commit a regression seed.** For JSON targets, copy the shrunk JSON
   into `fuzz/seeds/<target>/regression/<slug>.json` with `_about` and
   `_fix` fields. For byte targets, copy the minimized artifact into
   `fuzz/seeds/<target>/regression/<slug>` (no extension required).

6. **Verify.**

   ```bash
   cargo fmt && cargo clippy --workspace --all-targets -- -D warnings
   cargo nextest run --workspace
   cargo xtask fuzz replay-regression
   ```

   `replay-regression` replays every committed seed (JSON via
   `replay_scenario`, byte via `cargo fuzz run --runs=0`); a green run
   confirms the new seed locks down the fix. Any FAIL → back to step 4.

7. **Commit.** Conventional commits, single subject line, no body, no
   co-author. Two commits is fine: one for the fix, one for the seed.

8. **Relaunch only the crashed target** with the same `cargo xtask fuzz
   run` invocation from "Launch". Do not auto-relaunch on every
   notification — only after a fix is committed.

## Constraints

- No remote push, no `--force`, no rebase onto remote.
- Don't reference internal phase plans in commit messages.
- The notification is the wake signal; never sleep, poll, or
  `ScheduleWakeup` for fuzz progress.
