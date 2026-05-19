# smelt fuzz

Four targets covering distinct surfaces:

| target | what it fuzzes | how |
|---|---|---|
| `smelt_loop` | TUI event loop (terminal + engine events) | structured `Scenario` ops |
| `lua_loop` | Lua FFI bindings (`smelt.*` API) | structured `LuaScenario` ops + FFI ledger oracle |
| `text_ops` | `smelt_buffer::text` UTF-8 helpers | direct calls + reference-model differential |
| `attached_ops` | `smelt_buffer::attached::AttachedTextMut` | segment-based reference model + invariant check |

## Setup once

```sh
rustup toolchain install nightly
cargo install cargo-fuzz
```

## Day-to-day

```sh
# Run a target until you Ctrl-C, parallel workers, stop on first crash:
cargo +nightly fuzz run --sanitizer=none smelt_loop -- -fork=4 -ignore_crashes=0

# Unattended (keep going past crashes; artifacts pile up under fuzz/artifacts/):
cargo +nightly fuzz run --sanitizer=none lua_loop -- -fork=4 -ignore_crashes=1 -max_total_time=3600
```

## When the fuzzer finds a crash

```sh
# One-shot triage: shrink + format + print the minimal scenario.
fuzz/scripts/triage.sh lua_loop fuzz/artifacts/lua_loop/crash-<hex>
```

Then either fix the bug and commit a regression seed, or — if you want
to keep poking — replay the shrunk scenario directly:

```sh
cargo run --bin replay_scenario -- --target lua_loop /path/to/shrunk.json
# Or step through it visually (smelt_loop only):
cargo run --bin play_scenario -- /path/to/shrunk.json
```

## Regression seeds

Every fuzz-found bug gets a committed JSON seed under
`fuzz/seeds/<target>/regression/`. CI replays them on every PR; running
them locally is one command:

```sh
fuzz/scripts/replay-regression.sh
```

See `fuzz/seeds/README.md` for the convention and how to add one.

## Overnight

```sh
# Cmin first (sweeps the historical corpus for regressions; minimizes
# corpus for faster fuzz next session), then fuzz from the minimized
# corpus for the remaining time. Default budget 8 hours.
fuzz/scripts/overnight.sh [hours]
```

Logs land in `/tmp/fuzz-overnight-<stamp>/`. New crashes copy through to
`fuzz/artifacts/<target>/`.

## Coverage scoreboard

```sh
# Snapshot per-target source-code coverage to fuzz/coverage-history/.
# Use to A/B your own generator changes: snapshot before, change, snapshot
# after, diff the .txt files.
fuzz/scripts/coverage-snapshot.sh
fuzz/scripts/coverage-snapshot.sh smelt_loop      # one target only
```

## Lower-level

```sh
# Raw shrinker — predicate is `panic::catch_unwind(run_scenario)`.
cargo run --release --bin shrink_scenario -- --target lua_loop in.json out.min.json

# Headless replay (exits non-zero on panic; what triage.sh and CI use):
cargo run --bin replay_scenario -- --target lua_loop in.json

# Cmin a single target manually:
cargo +nightly fuzz cmin --sanitizer=none smelt_loop

# Source-code coverage report for one target:
cargo +nightly fuzz coverage --sanitizer=none smelt_loop fuzz/corpus/smelt_loop
```
