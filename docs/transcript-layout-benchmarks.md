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

## Active transcript retained memory

The active-memory suite builds an active canonical session in 256-block save
batches, applies each persistence receipt, and drains bounded idle compaction.
It then measures first render, 20 Ctrl-D scrolls, indexed search, `n` navigation,
and explicit hydration of up to 1,200 blocks. Reusing the newest hydrated block
must perform no additional SQLite read. The benchmark fails if committed full
content remains live, any cache exceeds its measured budget allowance, or a
50 MiB or larger workload does not exercise hydrated-block eviction.

Run it through xtask with an explicit generated byte target:

```bash
mkdir -p ~/tmp
TMPDIR=~/tmp cargo xtask bench-transcript-layout --runs 1 --skip-nav \
  --workloads tiny_blocks_1mib --active-memory \
  --active-memory-bytes 52428800

TMPDIR=~/tmp cargo xtask bench-transcript-layout --runs 1 --skip-nav \
  --workloads tiny_blocks_1mib --active-memory \
  --active-memory-bytes 524288000
```

The suite emits one `TRANSCRIPT_ACTIVE_MEMORY_JSON` object with every retained
category, budget, pin, oversize debt, hydration, eviction, dematerialization,
allocator, timing, RSS, and peak-RSS value. `TMPDIR` must point at a filesystem
with room for the canonical databases. Build the test executable before wrapping
it with `/usr/bin/time`; otherwise the external high-water mark includes rustc.
The 2026-07-20 Phase 5 measurements used the `release-fast` profile and these
commands after prebuilding:

```bash
TMPDIR=~/tmp cargo test --profile release-fast -p smelt-tui \
  --features harness transcript_active_memory_benchmark_suite \
  --no-run

test_bin="$(find target/release-fast/deps -maxdepth 1 -type f \
  -name 'tui-*' -perm -u+x -printf '%T@ %p\n' \
  | sort -nr | head -1 | cut -d' ' -f2-)"

/usr/bin/time -v env TMPDIR="$HOME/tmp" \
  SMELT_TRANSCRIPT_ACTIVE_MEMORY_BYTES=52428800 \
  "$test_bin" --exact \
  app::harness_tests::transcript_bench::transcript_active_memory_benchmark_suite \
  --ignored --nocapture

/usr/bin/time -v env TMPDIR="$HOME/tmp" \
  SMELT_TRANSCRIPT_ACTIVE_MEMORY_BYTES=524288000 \
  "$test_bin" --exact \
  app::harness_tests::transcript_bench::transcript_active_memory_benchmark_suite \
  --ignored --nocapture
```

Raw outputs were retained as
`~/tmp/phase5-active-memory-50m-release-fast-v4.log` and
`~/tmp/phase5-active-memory-500m-release-fast-v2.log`. The 50 MiB sample was
rerun after the final reflection refinements; its bounded full-content
categories and SQLite read counts remained unchanged.

The centralized defaults were 32 MiB for hydrated block content, 16 MiB for
loaded descriptor windows, and 16 MiB for rendered payloads. Both workloads
ended with zero live block and tool-state bytes, zero oversize debt, and zero
SQLite rereads when immediately reusing the newest hydration-churn block.

| Category | Active 50 MiB | Active 500 MiB |
|---|---:|---:|
| Generated bytes / blocks | 52,451,885 / 1,439 | 524,314,464 / 14,384 |
| Live full-content bytes | 0 | 0 |
| Hydrated block bytes | 33,536,786 | 33,536,788 |
| Stored / hydrated blocks | 524 / 915 | 13,469 / 915 |
| Compact descriptor bytes | 1,070,616 | 10,586,624 |
| Descriptor-window bytes | 0 | 0 |
| Tool-state metadata bytes | 0 | 0 |
| Origin/hash bytes | 28,672 | 458,752 |
| Rendered payload bytes | 256,801 | 1,188,844 |
| Hydrated / rendered pinned bytes | 73,310 / 183,564 | 73,312 / 1,115,606 |
| Hydration reads / ranges | 1,201 / 1,201 | 1,203 / 1,203 |
| Hydration bytes / duration | 44,019,149 / 120.844 ms | 44,092,456 / 1,427.513 ms |
| Evicted entries / bytes | 286 / 10,482,363 | 288 / 10,555,668 |
| Dematerialized entries / bytes | 1,439 / 52,742,563 | 14,384 / 527,220,032 |
| Allocator retained delta | 38,847,944 | 56,551,535 |
| In-process RSS / peak RSS | 123,580,416 / 123,580,416 | 145,805,312 / 145,805,312 |
| External peak RSS | 122,191,872 | 143,732,736 |

| Operation | Active 50 MiB | Active 500 MiB |
|---|---:|---:|
| Persist and compact fixture | 2,131.291 ms | 27,506.474 ms |
| First render | 2.358 ms | 9.251 ms |
| 20 Ctrl-D scrolls | 8.665 ms | 67.555 ms |
| Indexed search and reveal | 4.212 ms | 12.550 ms |
| `n` navigation and reveal | 2.431 ms | 8.000 ms |
| Hydrate up to 1,200 blocks | 232.902 ms | 2,188.738 ms |
| Working-set SQLite rereads | 0 | 0 |

The 500 MiB workload retained only two additional hydrated-content bytes. Its
additional retained memory came from compact per-block descriptors, mappings,
and allocator overhead: compact descriptors grew by about 9.1 MiB, allocator
retention by about 16.9 MiB, and in-process RSS by about 21.2 MiB. It did not
retain an additional 450 MiB copy of committed content. The individual
1,200-block hydration loop is deliberately adversarial and SQLite-latency-bound;
normal rendering and navigation hydrate coalesced viewport/range work. Compact
metadata remains proportional to block count, and fully active uncommitted model
history still remains proportional to its content until a durable receipt makes
it eligible for idle dematerialization.

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

### Canonical architecture Phase 0 smoke

Before changing Enter behavior, a 2026-07-20 release-mode smoke reran the 1K
submission and `tiny_blocks_1mib` workload with:

```bash
TMPDIR=~/tmp cargo xtask bench-transcript-layout --runs 1 --skip-nav \
  --workloads tiny_blocks_1mib --save-request-history 1000
```

Enter completed in 24.691 ms versus the retained 25.694 ms baseline. The layout
workload completed first projection in 11.036 ms and peaked at 83.6 MiB RSS. The
submission recorded exactly one canonical session-commit attempt and completion,
one synchronous metadata export, and the commit timestamp before provider
dispatch. The rewind sample also recorded 23,390 descriptor payload bytes as both
hydrated and pinned, with zero evicted bytes, matching the permanent `OnceLock`
cache baseline. The full command output was retained during implementation as
`~/tmp/smelt-phase0-release-smoke.log`.

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
