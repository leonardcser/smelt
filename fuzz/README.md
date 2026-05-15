# smelt fuzz target

cargo-fuzz target that drives a real `TuiApp` through `Arbitrary`-derived
event streams. Asserts text-buffer, registry, and resource invariants
after every event.

## Prerequisites

- Nightly toolchain (`rustup toolchain install nightly`) — libFuzzer
  instrumentation uses unstable `-Z sanitizer` flags.
- `cargo install cargo-fuzz`.

## Run

```bash
# from the fuzz/ directory
cargo +nightly fuzz run smelt_loop -- -max_len=4096 -max_total_time=300
```

- `-max_len=4096` lets libFuzzer mutate longer inputs once coverage
  stabilizes; the default is too small to exercise complex event
  sequences (engine streams, large pastes).
- `-max_total_time=N` caps wall time in seconds.
- `-runs=N` caps total iterations (useful for CI).

Coverage-bearing inputs accumulate under `corpus/smelt_loop/`. Crashes
land in `artifacts/smelt_loop/`; replay any one with:

```bash
cargo +nightly fuzz run smelt_loop artifacts/smelt_loop/<crash-file>
```

## What the target checks

Per event:

- Every `(Window, Buffer)` pair: cursor in `0..=source.len()` and on a
  UTF-8 char boundary.
- Selection anchors satisfy the same two constraints.
- Terminal width and height are non-zero.
- Per-event allocation budget (`AllocBudget::DEFAULT`: 10_000 allocs,
  4 MiB) is not exceeded.

Per iteration:

- Process-global theme + namespace registries do not grow more than
  `INTERN_SLACK` (64) past the post-build baseline. Unbounded growth
  across libFuzzer iterations surfaces as the smallest input that
  inflates either registry.

## Input shape

`FuzzInput { vim: bool, mode: FuzzMode, ops: Vec<FuzzOp> }` decoded via
`arbitrary`. `FuzzOp` covers:

- UTF-8 keystrokes (`KeyUnicode(u32)`), ctrl/shift modifiers, special
  keys (Enter/Esc/arrows/Home/End/PageUp/Down/Delete).
- Bracketed paste with arbitrary UTF-8 payload.
- Virtual clock advances (`Tick(ms)`).
- Lua wakeup pulses.
- Resizes (`width × height` clamped to `[1, 400]`).
- Engine stream events: `Ready`, `Text`, `TextDelta`,
  `ThinkingDelta`, `ToolStarted`, `ToolOutput`, `ToolFinished`.
- Foregrounded exec output and exit codes.

## Project layout

`smelt-fuzz` is intentionally outside the workspace (`fuzz/Cargo.toml`
declares its own `[workspace]`). Default workspace builds and tests
ignore it; the fuzz toolchain is only invoked when running this target.
