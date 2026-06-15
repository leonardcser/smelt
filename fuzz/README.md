# smelt fuzz

Fourteen targets covering distinct surfaces:

| target | what it fuzzes | how |
|---|---|---|
| `smelt_loop` | TUI event loop (terminal + engine events) | structured `Scenario` ops, swarm weighting, and macro workloads |
| `lua_loop` | Lua FFI bindings (`smelt.*` API) | structured `LuaScenario` ops + FFI ledger oracle |
| `text_ops` | `smelt_buffer::text` UTF-8 helpers | direct calls + reference-model differential |
| `attached_ops` | `smelt_buffer::attached::AttachedTextMut` | segment-based reference model + invariant check |
| `cache_invariance` | Anthropic prompt-cache prefix stability | random history + `cache_control`-aware byte diff |
| `openai_cache_invariance` | OpenAI / aux-model prompt-cache stability | random history + `prompt_cache_key`-aware byte diff |
| `snapshot_roundtrip` | `SnapshotFrame::parse` round-trip | random grid + style palette + assert `from_grid → text+styles → parse` is identity |
| `grid_invariants` | terminal grid mutation/diff invariants | random cell writes/fills + wide-char and diff-replay oracles |
| `ansi_parser` | ANSI SGR parser + wrapped emission | random bytes → lossy UTF-8 + `wrap_ansi` / `emit_ansi_row` UTF-8 boundary checks |
| `edit_ops` | editor/window core (`smelt_edit::Ui`, buffer edits, vim keys, mouse, resize) | focused shell around edit primitives + cursor/selection UTF-8 invariants |
| `transcript_render` | transcript-producing engine events and renderer projection | focused `TestApp` shell: tool/text/thinking/process events + resize/render invariant checks |
| `provider_body` | provider request-body construction | low-dependency Anthropic/OpenAI body builders + serialization/schema/cache-key smoke invariants |
| `provider_stream` | provider SSE/response parsers | pure SSE draining + Chat Completions/OpenAI/Anthropic stream and JSON parser summaries |
| `permissions_rules` | permission rule compilation/evaluation | random rule sets, shell-aware subpatterns, mode behavior, workspace downgrade oracle |

## Setup once

```sh
rustup toolchain install nightly
cargo install cargo-fuzz
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

Anything after the target name is forwarded to libFuzzer. The two flags
above the `--` shim live on the xtask itself:

| flag | what |
|---|---|
| `--fork N` | parallel workers (default 1) |
| `--cmin` | sweep + shrink the corpus first |
| trailing args | passed verbatim to libFuzzer (e.g. `-max_total_time=3600`) |

## Background fuzzing (agents)

The intended shape for long-running fuzz sessions in an agent loop:

1. Spawn each target with `run_in_background: true`, redirecting output to
   `/tmp/fuzz-loop/fuzz-<target>.log`.
2. Don't poll - the harness fires a notification the moment any background
   process exits, and `cargo xtask fuzz run` exits on crash, OOM, timeout,
   corpus preflight failure, or Ctrl-C. An exit code 77 means libFuzzer caught
   a panic; the artifact is under `fuzz/artifacts/<target>/`.
3. On notification: read the log, locate the failure artifact, triage:

   ```sh
   cargo xtask fuzz triage <target> fuzz/artifacts/<target>/<artifact>
   ```

4. Fix the bug, commit a regression seed under
   `fuzz/seeds/<target>/regression/`, then re-launch the target.

The agent doesn't sleep, schedule wake-ups, or check progress - the
notification on exit is the loop signal.

## When the fuzzer finds a crash

```sh
# One-shot triage: shrink + format + print the minimal scenario.
cargo xtask fuzz triage lua_loop fuzz/artifacts/lua_loop/crash-<hex>
```

Then either fix the bug and commit a regression seed, or - if you want
to keep poking - replay the shrunk scenario directly:

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
cargo xtask fuzz replay-regression
```

See `fuzz/seeds/README.md` for the convention and how to add one.

## Coverage scoreboard

```sh
cargo xtask fuzz status

# Snapshot per-target source-code coverage to fuzz/coverage-history/.
# Use to A/B your own generator changes: snapshot before, change, snapshot
# after, diff the .txt files.
cargo xtask fuzz coverage-snapshot
cargo xtask fuzz coverage-snapshot --timeout 120 smelt_loop      # one target only
```

## Lower-level

```sh
# Build real fuzz targets one-by-one (preferred over bare `cargo fuzz build`,
# which also tries to instrument helper binaries in this crate).
cargo xtask fuzz build

# Raw shrinker — predicate is `panic::catch_unwind(run_scenario)`.
cargo run --release --bin shrink_scenario -- --target lua_loop in.json out.min.json

# Headless replay (exits non-zero on panic; what triage and CI use):
cargo run --bin replay_scenario -- --target lua_loop in.json

# Cmin a single target manually:
cargo +nightly fuzz cmin --sanitizer=none smelt_loop

# Source-code coverage report for one target:
cargo +nightly fuzz coverage --sanitizer=none smelt_loop fuzz/corpus/smelt_loop
```
