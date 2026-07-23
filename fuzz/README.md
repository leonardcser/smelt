# smelt fuzz

Seventeen targets covering distinct surfaces:

| target                    | what it fuzzes                                                               | how                                                                                             |
| ------------------------- | ---------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| `smelt_loop`              | TUI event loop (terminal + engine events)                                    | structured `Scenario` ops, swarm weighting, and macro workloads                                 |
| `lua_loop`                | Lua FFI bindings (`smelt.*` API)                                             | structured `LuaScenario` ops + FFI ledger oracle                                                |
| `text_ops`                | `smelt_buffer::text` UTF-8 helpers                                           | direct calls + reference-model differential                                                     |
| `attached_ops`            | `smelt_buffer::attached::AttachedTextMut`                                    | segment-based reference model + invariant check                                                 |
| `cache_invariance`        | Anthropic prompt-cache prefix stability                                      | random history + `cache_control`-aware byte diff                                                |
| `openai_cache_invariance` | OpenAI / aux-model prompt-cache stability                                    | random history + `prompt_cache_key`-aware byte diff                                             |
| `snapshot_roundtrip`      | `SnapshotFrame::parse` round-trip                                            | random grid + style palette + assert `from_grid → text+styles → parse` is identity              |
| `grid_invariants`         | terminal grid mutation/diff invariants                                       | random cell writes/fills + wide-char and diff-replay oracles                                    |
| `ansi_parser`             | ANSI SGR parser + wrapped emission                                           | random bytes → lossy UTF-8 + `wrap_ansi` / `emit_ansi_row` UTF-8 boundary checks                |
| `edit_ops`                | editor/window core (`smelt_edit::Ui`, buffer edits, vim keys, mouse, resize) | focused shell around edit primitives + cursor/selection UTF-8 invariants                        |
| `transcript_render`       | transcript-producing engine events and renderer projection                   | focused `TestApp` shell: tool/text/thinking/process events + resize/render invariant checks     |
| `transcript_scroll_ops`   | sparse transcript scrolling, cursor motion, selection drag, and autoscroll   | resumed heterogeneous transcript + semantic scroll trace oracles                                |
| `provider_body`           | provider request construction and configuration                              | all body builders + routing, API-base, auth, catalog, extraction, and schema invariants          |
| `provider_stream`         | provider SSE/response parsers                                                | raw-byte partitioning + Chat Completions/OpenAI/Anthropic lifecycle and response oracles         |
| `permissions_rules`       | permission rule compilation/evaluation                                       | random rule sets, shell-aware subpatterns, mode behavior, workspace downgrade oracle            |
| `store_state`             | canonical session persistence and maintenance                                | file-backed writer/reader state machine + independent history, transcript-record, and revision model   |
| `engine_events`           | engine lifecycle and canonical-history application                           | focused event state machine + independent active-turn and canonical-suffix model                |

## Setup once

```sh
rustup toolchain install nightly
cargo install cargo-fuzz
```

Operational corpus, artifact, and coverage data lives outside the checkout under
`$XDG_CACHE_HOME/smelt/fuzz/<repository-id>/`. Every worktree for the same clone
uses that directory. Set `SMELT_FUZZ_HOME` to use an explicit location.
Regression seeds remain tracked under `fuzz/seeds/`. Initialize an empty shared
root with one bootstrap corpus input per registered target:

```sh
cargo xtask fuzz prepare
```

`run`, `coverage-snapshot`, and `verify` also prepare the corpora they need.
Import corpus and artifacts from the old checkout-local layout once:

```sh
cargo xtask fuzz import-data
# Or import from another clone or checkout:
cargo xtask fuzz import-data /path/to/repository/fuzz
```

## Day-to-day

Build all real fuzz targets without accidentally building helper bins:

```sh
cargo xtask fuzz build
cargo xtask fuzz build smelt_loop ansi_parser
```

Fuzz a single target until first crash/OOM/timeout or Ctrl-C:

```sh
cargo xtask fuzz run smelt_loop --fork 8
```

AddressSanitizer is the default. Use `--sanitizer none` only for a deliberate
high-throughput run, not as the only campaign for a target.

| flag               | what                                                       |
| ------------------ | ---------------------------------------------------------- |
| `--fork N`         | parallel workers (default 1)                               |
| `--cmin`           | sweep and shrink the shared corpus first                    |
| `--sanitizer KIND` | address, leak, memory, thread, or none                       |
| trailing args      | passed verbatim to libFuzzer, such as `-max_total_time=3600` |

## Background fuzzing (agents)

The intended shape for long-running fuzz sessions in an agent loop:

1. Spawn each target with `run_in_background: true`, redirecting output to
   `/tmp/fuzz-loop/fuzz-<target>.log`.
2. Don't poll - the harness fires a notification the moment any background
   process exits, and `cargo xtask fuzz run` exits on crash, OOM, timeout,
   corpus preflight failure, or Ctrl-C. An exit code 77 means libFuzzer caught a
   panic. The command prints the shared artifact directory on failure; `cargo
   xtask fuzz status` also prints the shared data root.
3. On notification: read the log, locate the failure artifact, and triage it:

   ```sh
   cargo xtask fuzz triage <target> /shared/fuzz/data/artifacts/<target>/<artifact>
   ```

4. Fix the bug, commit a regression seed under
   `fuzz/seeds/<target>/regression/`, then re-launch the target.

The agent doesn't sleep, schedule wake-ups, or check progress - the notification
on exit is the loop signal.

## When the fuzzer finds a crash

```sh
# Structured targets preserve panic identity while shrinking JSON operations.
cargo xtask fuzz triage lua_loop /shared/fuzz/data/artifacts/lua_loop/crash-<hex>

# Byte targets minimize and replay under AddressSanitizer, then verify the fingerprint.
cargo xtask fuzz triage provider_stream /shared/fuzz/data/artifacts/provider_stream/crash-<hex>
```

Triage writes the minimized artifact and a `.triage.json` metadata sidecar next
to the original. Then either fix the bug and commit a regression seed, or replay
the minimized scenario directly:

```sh
cargo run --features scenario-tools --bin replay_scenario -- \
  --target lua_loop /path/to/shrunk.json
# Or step through it visually (smelt_loop only):
cargo run --features scenario-tools --bin play_scenario -- /path/to/shrunk.json
```

## Regression seeds

Every fuzz-found bug gets a committed seed under
`fuzz/seeds/<target>/regression/`. Structured targets use JSON scenarios and
byte targets keep the exact minimized artifact. CI replays both forms on every
PR; running them locally is one command:

```sh
# Build every target, then replay all regression seeds:
cargo xtask fuzz replay-regression

# CI gate: prepare, compile, and replay every registered target:
cargo xtask fuzz verify
```

Seed presence never controls whether a target is compiled. The target registry
in `crates/xtask/src/fuzz/mod.rs` is checked against `fuzz/Cargo.toml` before any
fuzz command runs. See `fuzz/seeds/README.md` for the regression convention.

## Coverage scoreboard

```sh
cargo xtask fuzz status

# Snapshot per-target source-code coverage to shared coverage-history/.
# Each run writes a text report plus JSON containing the commit, corpus digest,
# input count, status, and llvm-cov totals.
cargo xtask fuzz coverage-snapshot
cargo xtask fuzz coverage-snapshot --timeout 120 smelt_loop      # one target only
```

## Lower-level

```sh
# Build every registered fuzz target in one cargo-fuzz invocation.
cargo xtask fuzz build

# Raw structured shrinker; the predicate preserves normalized panic identity.
cargo run --release --features scenario-tools --bin shrink_scenario -- \
  --target lua_loop in.json out.min.json

# Headless replay (exits non-zero on panic; what triage and CI use):
cargo run --features scenario-tools --bin replay_scenario -- \
  --target lua_loop in.json

# Prefer the wrappers so custom corpus and artifact paths are always supplied:
cargo xtask fuzz run smelt_loop --cmin --sanitizer none -max_total_time=3600
cargo xtask fuzz coverage-snapshot smelt_loop
```
