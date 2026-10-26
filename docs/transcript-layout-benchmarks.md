# Transcript benchmarks

Benchmarks are feature-gated release tests. Most run through
`cargo xtask bench-transcript-layout`; a few store and TUI suites run directly
with `cargo test`. All of them build isolated fixtures. Never benchmark against a
live sessions root: copy one with SQLite's online backup API first.

Put `TMPDIR` under `~/tmp` for 50 MiB and larger fixtures so temporary databases
do not fill `/tmp`. Use `--debug` only for instrumentation work, never for timing
baselines.

## Layout, navigation, and scroll pills

```bash
cargo xtask bench-transcript-layout --runs 5
cargo xtask bench-transcript-layout --runs 10 --workloads mixed_10mib
cargo xtask bench-transcript-layout --runs 1 --workloads mixed_50mib --skip-nav
cargo xtask bench-transcript-layout --runs 5 --workloads markdown_4mib,tool_output_4mib
```

Reports per-workload projection and cache timings, structural counters (layout
compilation, exact height measurement, visible materialization), and unless
`--skip-nav`, app-level timings for `/` search, Ctrl-D, Ctrl-U, `gg`, `G`, and
real mouse clicks on the previous-user and bottom scroll pills over a
4,000-turn, 28,117-row fixture.

The pill and navigation paths reject full-session reads, full block-buffer
renders, complete height-index rebuilds, more than 128 exactified nodes, and more
than 1,024 materialized rows.

## Session-size scaling

```bash
cargo xtask bench-transcript-layout --scaling
cargo xtask bench-transcript-layout --scaling --scaling-blocks 2000,32000 --runs 48
cargo xtask bench-transcript-layout --scaling --scaling-compacted
```

Sweeps transcript block count (default `2000,8000,32000`) and reports the median
cost of interactions that should not depend on session size, plus the fitted
exponent between the smallest and largest session (about 0 is constant, about 1
is linear). `--scaling-compacted` saves every 256 blocks and dematerializes the
fixture first, and also reports on-disk bytes per block and each save duration.
`SMELT_TRANSCRIPT_SCALING_PERF=1` prints the top perf spans per operation.

Operations: `prompt_key`, `idle_frame`, `gg_then_G`, `ctrl_d`, `fold_mid`,
`resize_width`, `search_key` (one incremental search keystroke), `append_block`,
`debug_panel_toggle` and `perf_panel_toggle` (open and close the F3 and F12
panels), `debug_panel_tick` and `perf_panel_tick` (one timer repaint with the
panel open), `stream_delta`, and `spinner_frame`.

Expected: every operation under 1 ms at 32,000 blocks with an exponent of 0.25 or
less. Unsaved `search_key` is the one linear term (one substring scan over cached
indexed text per keystroke, about 0.85 ms at 32,000 blocks).

## Tall expanded `write_file` and diffs

```bash
cargo xtask bench-transcript-layout --runs 5 --tall-write-only --tall-write-lines 20000
cargo xtask bench-transcript-layout --runs 5 --tall-diff-only --tall-diff-lines 20000
```

Expands a completed `write_file` with a real Enter and scrolls 12 wheel frames at
the top, middle, and 90 percent depth. Deep scrolls must cost the same as
shallow ones: each frame rejects full renders and index rebuilds, more than 3,072
materialized rows, 128 or more replayed prefix source lines, and more than 256
highlighted source lines.

## Streaming

```bash
cargo xtask bench-transcript-layout --runs 5 --stream-only \
  --stream-workload bash --stream-scheduled --stream-history 5 \
  --stream-chunks 512 --stream-bytes 65536

cargo xtask bench-transcript-layout --runs 1 --stream-only \
  --stream-workload explore-group --stream-scheduled \
  --stream-parallel-tools 8 --stream-chunks 16 --stream-bytes 1048576

# Autonomous frames after a sparse 5 MiB tail stops streaming.
cargo xtask bench-transcript-layout --runs 1 --stream-only \
  --stream-workload text --stream-scheduled --stream-resumed-bytes 5242880 \
  --stream-resumed-position tail --stream-chunks 1 --stream-bytes 1 \
  --stream-idle-frames 100

# Large records at persisted extent boundaries.
cargo xtask bench-transcript-layout --runs 1 --stream-only \
  --stream-workload text --stream-scheduled --stream-resumed-bytes 1 \
  --stream-resumed-position tail --stream-boundary-record-bytes 524288 \
  --stream-chunks 1 --stream-bytes 1 --stream-idle-frames 20

# Navigation while streaming into a large prior transcript.
cargo xtask bench-transcript-layout --runs 5 --stream-only --stream-scroll \
  --stream-workload text --stream-history 10000 \
  --stream-chunks 512 --stream-bytes 16384
```

Workloads: `text`, `bash` (provider tool output), `mixed` (reasoning, text, and
bash in one turn), `exec` (local `!command`), `write-draft` (incremental JSON
draft preview), and `explore-group` (parallel grouped tools). The default mode
redraws after every event as an upper bound; `--stream-scheduled` uses the
production frame scheduler and `--stream-scroll` adds urgent navigation.

Reports dispatch and frame tails, request-to-flush p99, allocations, and perf
rows. Scale response bytes to find work proportional to accumulated output,
chunk count to find per-event amplification, and history to find global scans.

Gates:

- input-to-frame below 16.7 ms p95 and 33 ms p99 while streaming;
- streaming work independent of prior transcript block count;
- a 64 KiB response in 2,048 chunks allocates under 16 MiB in the warmed hot path;
- ordinary streaming frames capped at 60 per second, with first output, final
  output, and interaction frames urgent.

## Submit, persistence, and provider history

```bash
cargo xtask bench-transcript-layout --runs 3 --skip-nav \
  --workloads tiny_blocks_1mib --save-request-history 10000

TMPDIR=~/tmp cargo xtask bench-transcript-layout --runs 20 \
  --save-request-only --save-request-operations submit_enter \
  --save-request-history 1024 --save-request-heterogeneous

# Replay a copied sessions root through the normal resume and Enter paths.
TMPDIR=~/tmp cargo xtask bench-transcript-layout --runs 20 \
  --save-request-fixture ~/tmp/session-fixtures/sessions \
  --save-request-session-id <session-id> \
  --save-request-operations submit_enter

TMPDIR=~/tmp cargo xtask bench-transcript-layout --runs 1 --skip-nav \
  --workloads tiny_blocks_1mib --save-request-history 2000 \
  --save-request-item-bytes 8192
```

Measures `submit_enter` (Enter through durable persistence and `StartTurn`),
`submit_first_render`, no-op save, request and history appends, turn completion,
rewind, checkpointed and uncheckpointed provider history loading, and
`engine_request_materialization`. Each sample splits queue wait, `submit_turn`,
commit, `agent:begin_turn`, and project-context preparation, with allocation and
retained-byte counts. Fixture copy and resume are outside the measured interval.

Enter must stay proportional to the appended suffix, not the history length.
Provider request construction is inherently linear in uncheckpointed active
history. Compare peak RSS only across separate invocations, since it is a
process high-water mark.

## Search and resume

```bash
TMPDIR=~/tmp cargo xtask bench-transcript-layout --runs 1 --skip-nav \
  --workloads tiny_blocks_1mib --search --search-bytes 52428800

TMPDIR=~/tmp cargo xtask bench-transcript-layout --runs 1 --skip-nav \
  --workloads tiny_blocks_1mib --resume --resume-bytes 52428800 --resumed-wheel

# 500 MiB stress target; warmup is disabled automatically.
TMPDIR=~/tmp cargo xtask bench-transcript-layout --runs 1 --skip-nav \
  --workloads tiny_blocks_1mib --scale-500mb
```

Search setup waits for the derived search projection before timing. Rare,
common, and absent queries are all measured because latency scales with posting
and candidate cardinality. A measured search fails unless it used the persisted
candidate index and current root manifest. Two-character literal searches still
scan indexed text and are the known unindexed weak spot.

## Active transcript memory

```bash
TMPDIR=~/tmp cargo xtask bench-transcript-layout --runs 1 --skip-nav \
  --workloads tiny_blocks_1mib --active-memory --active-memory-bytes 52428800
```

Builds an active session in 256-block saves, drains compaction, then measures
first render, Ctrl-D, indexed search, `n`, and one 1,200-block hydration request
larger than the hydration budget. Emits `TRANSCRIPT_ACTIVE_MEMORY_JSON`. Fails if
committed content stays live, a cache exceeds its budget, search skips the index,
or a 50 MiB or larger workload does not exercise hydrated-block eviction.

Retained memory should grow only by compact per-block records between 50 MiB and
500 MiB; hydrated content stays at its 32 MiB budget. Prebuild the test binary
before wrapping it with `/usr/bin/time -v`, or rustc dominates peak RSS.

## Startup and shutdown

```bash
cargo test --profile release-fast --test startup \
  interactive_lifecycle_benchmark_suite --no-run

SMELT_LIFECYCLE_BENCH_TARGET=1 \
SMELT_LIFECYCLE_BENCH_FIXTURE="$HOME/tmp/session-fixtures/sessions" \
SMELT_LIFECYCLE_BENCH_SESSION_ID='<session-id>' \
SMELT_LIFECYCLE_BENCH_READY_TEXT='<unique visible tail text>' \
SMELT_LIFECYCLE_BENCH_RUNS=10 \
cargo test --profile release-fast --test startup \
  interactive_lifecycle_benchmark_suite -- --nocapture
```

Launches the real executable in a PTY against a fresh copy per run and reports
launch to first frame, launch to visible tail, Ctrl-C to exit, and RSS.
`SMELT_LIFECYCLE_BENCH_FIRST_FRAME_TEXT` and
`SMELT_LIFECYCLE_BENCH_TIMEOUT_SECS` override the first-frame marker and phase
timeout. Resume must stay bounded by the loaded tail, not history length or
database size.

## Direct suites

```bash
# Sparse store indexes at 10K, 100K, and 1M records; must load zero payloads.
SMELT_SPARSE_INDEX_BENCH=1 SMELT_SPARSE_INDEX_BENCH_RUNS=1001 \
cargo test --release -p smelt-store sparse_index_scaling_benchmark_suite \
  -- --ignored --nocapture

# Sparse watcher dispatch; must reuse one metadata and one hydration reader.
SMELT_TRANSCRIPT_BENCH_TARGET=1 SMELT_TRANSCRIPT_SPARSE_WATCHER_BENCH=1 \
cargo test --release -p smelt-tui --features harness,transcript-bench \
  transcript_sparse_watcher_benchmark_suite -- --nocapture

# Sparse versus hydrated autonomous frames.
SMELT_TRANSCRIPT_BENCH_TARGET=1 SMELT_TRANSCRIPT_SPARSE_AUTONOMOUS_BENCH=1 \
cargo test --release -p smelt-tui --features harness,transcript-bench \
  transcript_sparse_autonomous_frame_benchmark_suite -- --nocapture

# Session catalog queries at 1K and 100K rows.
SMELT_CATALOG_BENCH=1 SMELT_CATALOG_BENCH_ROWS=100000 SMELT_CATALOG_BENCH_RUNS=101 \
cargo test --release -p smelt-store catalog_query_benchmark_suite \
  -- --ignored --nocapture
```
