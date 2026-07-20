# Transcript layout benchmarks

Run the transcript layout benchmark suite through xtask:

```bash
cargo xtask bench-transcript-layout --runs 5
```

The command runs ignored `smelt-tui` benchmark tests in release mode and prints:

- one sample line per workload/run;
- a mean±standard-deviation table for projection/cache workloads;
- structural counters for layout compilation, exact height measurement, and
  visible materialization;
- app-level navigation/search timings for `/`, Ctrl-D, Ctrl-U, `gg`, and `G`
  over a warmed transcript unless `--skip-nav` is passed.

Useful variants:

```bash
# Run only the 10 MiB mixed workload.
cargo xtask bench-transcript-layout --runs 10 --workloads mixed_10mib

# Run the 50 MiB mixed workload without the navigation suite.
cargo xtask bench-transcript-layout --runs 1 --workloads mixed_50mib --skip-nav

# Run a comma-separated subset.
cargo xtask bench-transcript-layout --runs 5 --workloads markdown_4mib,tool_output_4mib

# Debug/test profile for instrumentation work; do not use for timing baselines.
cargo xtask bench-transcript-layout --debug --runs 1 --workloads mixed_10mib
```

Current projection workloads:

- `mixed_10mib`: representative mixed transcript with user, assistant markdown,
  thinking, exec, and tool blocks.
- `mixed_50mib`: same shape at 50 MiB for large-resume/projection investigation;
  prefer `--runs 1 --skip-nav` while iterating because a 5-run sample is long.
- `markdown_4mib`: markdown-heavy assistant content with tables, code fences,
  quotes, and wrapping pressure.
- `tool_output_4mib`: many completed tool calls with long raw output bodies.
- `tiny_blocks_1mib`: many small heterogeneous blocks to expose per-block
  overhead.
- `huge_blocks_4mib`: few very large markdown/preformatted blocks to expose
  per-block measurement cost.

The app-level navigation/search benchmark uses an 8,000-block warmed transcript
and reports:

- `/needle-target` search submission plus redraw;
- common one-character and absent one-character fallback searches;
- `/common-token` submission and 100 `n` result jumps plus redraws;
- append-then-search timing for incremental search-index invalidation;
- 20 Ctrl-D half-page moves plus redraws;
- 20 Ctrl-U half-page moves plus redraws;
- `gg` plus redraw;
- `G` plus redraw;
- sparse/resumed 80-key burst scroll timings without scroll trace enabled,
  reported with the `prod_burst_*` prefix;
- sparse/resumed 80-key burst scroll timings with scroll trace enabled, reported
  with the `burst_*` prefix for diagnostic projection-frame counters;
- sparse/resumed search submission and 100 `n` result jumps, reported with the
  `sparse_*` prefix.

The xtask command forces `--test-threads=1` so global perf/allocation counters
and CPU contention from the projection and navigation suites do not contaminate
each other. Each benchmark also runs one unreported warmup sample before
collecting reported runs. Pass `--skip-nav` when investigating large projection
workloads where navigation/search is unrelated noise.

Use release numbers for decisions. Use the counters to identify algorithmic
changes: scroll/visible/full-cache paths should not compile layouts or remeasure
exact block heights, while no-cache cold projection should compile and measure
every block.

## Submit, persistence, and provider history

The save/request suite includes the actual prompt interaction instead of only
calling internal persistence methods:

```bash
# Row-count scaling with short messages.
cargo xtask bench-transcript-layout --runs 3 --skip-nav \
  --workloads tiny_blocks_1mib --save-request-history 10000

# Byte and memory scaling with 2,000 history items of at least 8 KiB each.
mkdir -p ~/tmp
TMPDIR=~/tmp cargo xtask bench-transcript-layout --runs 1 --skip-nav \
  --workloads tiny_blocks_1mib --save-request-history 2000 \
  --save-request-item-bytes 8192
```

The suite reports:

- `submit_enter`: pressing Enter through durable persistence and `StartTurn`
  dispatch;
- `submit_first_render`: the first incremental redraw after submission;
- no-op save, canonical request append, engine history append, turn completion,
  and short-suffix rewind;
- checkpointed provider history loading with a 32-row live suffix;
- uncheckpointed provider history loading across the complete active history;
- `engine_request_materialization`, which loads uncheckpointed active history and
  measures engine installation, one shared history-to-message conversion, and
  allocation-free token-estimate JSON counting. The same prepared message vector
  is reused by the provider when the prepare hook does not mutate history.

Each hot-path sample includes calling-thread allocations, process-wide allocation
and deallocation churn, retained allocator bytes before and after the operation,
full invariant-validation row count, derived search-blob rows and bytes, and the
number and bytes of user turns cloned during submission. The process-level
`BENCH_MEMORY_SUMMARY` reports peak RSS for the complete isolated benchmark
process.

Use a dedicated invocation for each size when comparing peak RSS. Peak RSS is a
process high-water mark and cannot be attributed to a later phase after a larger
fixture has already run. Put `TMPDIR` under `~/tmp` for 50 MiB and 500 MiB runs so
large temporary databases and spill files do not consume the limited `/tmp`
filesystem.

## Large search and resume

```bash
# 50 MiB indexed search.
TMPDIR=~/tmp cargo xtask bench-transcript-layout --runs 1 --skip-nav \
  --workloads tiny_blocks_1mib --search --search-bytes 52428800

# True sparse resume plus resumed wheel scrolling.
TMPDIR=~/tmp cargo xtask bench-transcript-layout --runs 1 --skip-nav \
  --workloads tiny_blocks_1mib --resume --resume-bytes 52428800 \
  --resumed-wheel

# 500 MiB stress target. Warmup is disabled automatically.
TMPDIR=~/tmp cargo xtask bench-transcript-layout --runs 1 --skip-nav \
  --workloads tiny_blocks_1mib --scale-500mb
```

Search queries of at least three characters use SQLite FTS5 trigram candidates.
Covered one-character queries use a compact character-presence table before
loading candidate text. Two-character queries and characters outside the compact
range use a literal table scan with a bounded result page. Rare and common
queries are both benchmarked because common-token latency scales with candidate
cardinality, not only total transcript bytes.

The true-resume sample reports allocator churn and retained bytes around only the
sparse tail load and first render. Whole-process peak RSS also includes fixture
construction and is therefore only an upper bound for resumed-session memory.

## Large-session performance results

The following single-run release measurements compare the original paths with
the optimized implementation. Each hot-path size ran in an isolated process with
`TMPDIR=~/tmp`; peak RSS is therefore comparable within each row.

| Workload | Enter before | Enter after | First redraw before | First redraw after | Engine materialization before | Engine materialization after | Peak RSS before | Peak RSS after |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1K short rows | 35.601 ms | 25.694 ms | 0.769 ms | 0.484 ms | 2.214 ms | 2.452 ms | 32.2 MiB | 34.6 MiB |
| 10K short rows | 44.050 ms | 29.083 ms | 2.499 ms | 0.675 ms | 11.972 ms | 12.452 ms | 76.2 MiB | 88.2 MiB |
| 50K short rows | 98.958 ms | 45.579 ms | 14.059 ms | 1.804 ms | 59.810 ms | 51.180 ms | 263.2 MiB | 287.6 MiB |
| 2K rows x 8 KiB | 80.901 ms | 36.840 ms | 1.131 ms | 0.461 ms | 27.800 ms | 23.620 ms | 334.1 MiB | 337.2 MiB |

At 50K rows, Enter is 54% faster and first redraw is 87% faster. At
2K x 8 KiB, Enter is 54% faster and its allocation churn falls from 80.77 MiB
to 12.96 MiB. Engine materialization allocation churn falls from 103.33 MiB to
72.22 MiB at 50K rows and from 97.94 MiB to 49.74 MiB for the byte-heavy case.
The small row-heavy peak-RSS increase is transient fixture/index construction;
steady allocator state is unchanged and the byte-heavy peak is within 1%.

Submission now performs no full history invariant scan, no `content.txt`
regeneration, and no user-turn cloning in the Enter barrier. The last-user lookup
scanned one block in every measured workload. The first redraw reused the render
plan and allocated only 1.33 MiB total at 50K rows, down from 8.06 MiB; the
render-plan update itself allocated 448 bytes.

### Search scaling

| Search workload | 50 MiB before | 50 MiB after | 500 MiB before | 500 MiB after |
|---|---:|---:|---:|---:|
| Absent one-character submit | 85.428 ms | 1.214 ms | 874.682 ms | 11.699 ms |
| Common FTS submit | 43.024 ms | 2.163 ms | 769.245 ms | 3.349 ms |
| Sparse common FTS submit | 87.409 ms | 2.863 ms | 854.983 ms | 4.001 ms |
| Append then repeat search | 19.470 ms | 1.277 ms | 189.086 ms | 11.607 ms |

FTS queries now retain rowid order instead of building a temporary sort.
One-character searches scan a compact presence table as the forced outer query
loop, then load text only for candidates. The compact table occupied about
0.5 MiB in the 234 MiB 50 MiB-search database. Two-character literal searches
remain linear in indexed text bytes and are the main unindexed search weak spot.

The 50 MiB sparse resume improved from 3.102 ms load plus 7.088 ms first render
to 1.824 ms plus 4.573 ms. Retained memory remained bounded at about 0.84 MiB.
For 240 resumed wheel frames, descriptor aggregate time fell from 111.667 ms to
57.191 ms and wall time fell from 1004.463 ms to 889.067 ms.

### Complexity and remaining limits

- Enter persistence is proportional to the changed history and descriptor
  suffix, plus SQLite commit and synchronous `meta.json` durability costs.
- First redraw after an append is proportional to the appended render-plan
  suffix and visible projection work. A bounded mutation log falls back to a
  full rebuild after it can no longer prove incremental safety.
- Provider request construction is necessarily linear in active model-history
  bytes, but the engine now performs one message conversion, no token-estimate
  byte-buffer allocation, and no unchanged-prefix snapshot clone.
- FTS search scales with posting and candidate cardinality. Covered
  one-character search scans compact per-block masks rather than transcript
  text. Two-character and out-of-range one-character searches still scan text.
- Sparse resume memory is proportional to the descriptor window and rendered
  tail, not total transcript size. Fully active, uncheckpointed model history
  remains linear in active history size and should be checkpointed for very
  large sessions.
- `content.txt` remains linear in transcript bytes, but it is disposable,
  coalesced off the Enter barrier, and streamed to an atomic temporary file with
  memory bounded by one indexed row. `meta.json` stays synchronous because
  session listing depends on its revision.
