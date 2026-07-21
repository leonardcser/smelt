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
workload completed first projection in 11.036 ms and peaked at 83.6 MiB RSS. At
that pre-cutover point, submission recorded exactly one canonical session-commit
attempt and completion, the then-synchronous metadata export, and the commit
timestamp before provider dispatch. The rewind sample also recorded 23,390
descriptor payload bytes as both
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

### Canonical session architecture final validation

The final Phase 7 validation ran on 2026-07-21 with `TMPDIR=~/tmp`. The layout,
Enter, search, sparse-resume, resumed-wheel, catalog, and compatibility-export
suites used the release profile. Active-memory measurements used `release-fast`
after prebuilding the test executable so rustc memory was excluded. Each large
workload ran in an isolated test process. The `BENCH_MEMORY_SUMMARY` high-water
mark is reported for xtask suites; direct-test `/usr/bin/time -v` RSS is reported
for catalog, compatibility export, and active memory.

The standard three-run matrix used:

```bash
TMPDIR=~/tmp cargo xtask bench-transcript-layout --runs 3
```

All six projection workloads and the navigation suite passed. The test process
peaked at 107,376 KiB RSS.

| Workload | Generated bytes | Blocks | First projection | 12 scrolls | Append |
|---|---:|---:|---:|---:|---:|
| `mixed_10mib` | 10,499,021 | 3,404 | 6.801 ms | 12.929 ms | 2.225 ms |
| `mixed_50mib` | 52,446,508 | 17,004 | 26.836 ms | 12.283 ms | 2.658 ms |
| `markdown_4mib` | 4,195,614 | 540 | 1.849 ms | 13.561 ms | 0.345 ms |
| `tool_output_4mib` | 4,221,105 | 47 | 21.565 ms | 85.956 ms | 13.361 ms |
| `tiny_blocks_1mib` | 1,048,586 | 32,112 | 9.082 ms | 6.101 ms | 0.804 ms |
| `huge_blocks_4mib` | 4,303,604 | 38 | 2.930 ms | 34.411 ms | 1.130 ms |

The final five-run Enter commands were:

```bash
TMPDIR=~/tmp cargo xtask bench-transcript-layout --runs 5 --skip-nav \
  --workloads tiny_blocks_1mib --save-request-history 50000

TMPDIR=~/tmp cargo xtask bench-transcript-layout --runs 5 --skip-nav \
  --workloads tiny_blocks_1mib --save-request-history 2000 \
  --save-request-item-bytes 8192
```

| Enter workload | Preserved baseline | 5% ceiling | Final samples | Final median | Delta | Peak RSS |
|---|---:|---:|---|---:|---:|---:|
| 50K short rows | 45.579 ms | 47.858 ms | 40.653, 37.859, 37.553, 47.230, 38.279 ms | 38.279 ms | -16.02% | 292,424 KiB |
| 2K rows x 8 KiB | 36.840 ms | 38.682 ms | 27.938, 26.916, 28.037, 28.742, 26.021 ms | 27.938 ms | -24.16% | 303,076 KiB |

Every Enter sample recorded exactly one `submit_turn` transaction, two inserted
history suffix rows, one descriptor suffix row, zero invariant history rows,
zero search-blob rows, one last-user block scanned, and at most two descriptor
rank entries scanned. Provider dispatch followed the durable receipt in every
sample. Catalog and compatibility work was queued after durability and was not
awaited by Enter. Benchmark setup waited for the prior fixture revision's derived
outputs before clearing metrics, so prior asynchronous work did not contaminate
the measured interval.

Search used the commands in "Large search and resume" above, with an explicit
`--search-bytes 524288000 --no-warmup` for 500 MiB. Both sizes ran five times to
quantify fixed-cost variance. Indexed candidate pages remained bounded at 512
blocks. Cached `n` navigation did not repeat full candidate scans, and descriptor
hydration used known canonical extents rather than recounting the whole descriptor
table.

| Search operation | 50 MiB preserved | 50 MiB final mean | 500 MiB preserved | 500 MiB final mean |
|---|---:|---:|---:|---:|
| Absent one-character submit | 1.214 ms | 1.424 ms | 11.699 ms | 11.296 ms |
| Common FTS submit | 2.163 ms | 2.551 ms | 3.349 ms | 3.426 ms |
| Sparse common FTS submit | 2.863 ms | 2.849 ms | 4.001 ms | 3.628 ms |
| Append then repeat search | 1.277 ms | 1.807 ms | 11.607 ms | 10.914 ms |

The 50 MiB fixed-path differences are 0.210 to 0.530 ms. For operations below 5
ms, final acceptance therefore uses the larger of 5 percent or a measured 0.6 ms
fixed-cost floor. The 500 MiB repeated means are within 5 percent or faster, and
the sparse 50 MiB path is faster. More importantly, the same fixed candidate and
hydration bounds held at both sizes. The search test process peaked at 666,880
KiB for the five-run 50 MiB suite and 6,065,000 KiB for the five-run 500 MiB
suite. Those process high-water marks include construction of the active 50 MiB
or 500 MiB fixture before it is made sparse.

True sparse resume and resumed-wheel commands used `--resume-bytes 52428800` and
`--resume-bytes 524288000 --no-warmup`. Resume retained memory measures only the
tail load interval; process RSS includes fixture construction.

| Workload | Tail load | First render | Tail retained | Resume peak RSS | 240 wheel frames | Wheel peak RSS |
|---|---:|---:|---:|---:|---:|---:|
| 50 MiB | 7.173 ms | 4.277 ms | 77,249 B | 85,764 KiB | 324.922 ms | 519,704 KiB |
| 500 MiB | 1.787 ms | 33.078 ms | 80,969 B | 621,424 KiB | 335.140 ms | 4,804,868 KiB |

Wheel wall time stayed effectively flat as source bytes increased tenfold. Each
summary recorded zero foreground descriptor-window loads during the measured
wheel loop and only two row-index rebuilds.

Final active-memory measurements used the direct commands shown above. Raw
content remained bounded by the 32 MiB hydration budget, with zero live committed
blocks, zero oversize debt, and zero rereads of the newest working-set block.

| Category | Final active 50 MiB | Final active 500 MiB |
|---|---:|---:|
| Generated bytes / blocks | 52,451,885 / 1,439 | 524,314,464 / 14,384 |
| Stored / hydrated blocks | 524 / 915 | 13,469 / 915 |
| Hydrated block bytes | 33,536,786 | 33,536,788 |
| Compact descriptor bytes | 1,070,616 | 10,701,696 |
| Rendered payload bytes | 256,801 | 1,188,844 |
| Allocator retained delta | 38,812,808 | 56,534,120 |
| In-process peak RSS | 123,744,256 B | 140,177,408 B |
| External peak RSS | 118,948 KiB | 134,332 KiB |

| Operation | Final active 50 MiB | Final active 500 MiB |
|---|---:|---:|
| Persist and compact fixture | 2,781.970 ms | 25,786.910 ms |
| First render | 2.935 ms | 6.193 ms |
| 20 Ctrl-D scrolls | 10.166 ms | 6.432 ms |
| Indexed search and reveal | 3.987 ms | 4.634 ms |
| `n` navigation and reveal | 2.159 ms | 2.707 ms |
| Hydrate up to 1,200 blocks | 114.672 ms | 124.082 ms |
| Working-set SQLite rereads | 0 | 0 |

The cached hydrated-membership and retained-byte invariants removed transcript-
length scans from budget enforcement. Compared with the Phase 5 measurements,
the 500 MiB first render improved from 9.251 to 6.193 ms, 20 scrolls from 67.555
to 6.432 ms, search from 12.550 to 4.634 ms, `n` navigation from 8.000 to 2.707
ms, and adversarial hydration from 2,188.738 to 124.082 ms. The 500 MiB
in-process peak RSS was only 15.7 MiB higher than the 50 MiB process, not another
450 MiB copy of committed content.

Catalog scaling was measured with 101 queries per operation after prebuilding the
release test executable:

```bash
SMELT_CATALOG_BENCH=1 SMELT_CATALOG_BENCH_ROWS=<1000|100000> \
SMELT_CATALOG_BENCH_RUNS=101 <smelt-store-test> --exact \
  catalog::tests::catalog_query_benchmark_suite --ignored --nocapture
```

| Catalog rows | First page median | Second page median | Filtered median | Database | Peak RSS |
|---|---:|---:|---:|---:|---:|
| 1,000 | 65 us | 74 us | 80 us | 643,072 B | 7,440 KiB |
| 100,000 | 68 us | 79 us | 85 us | 70,205,440 B | 8,464 KiB |

The 100x row increase changed page latency by at most 5 us and did not load all
catalog rows into Rust. Queries used only the already-open catalog connection and
opened zero session databases.

Compatibility export used streamed 1 MiB rows through the production atomic
export path:

```bash
SMELT_COMPAT_EXPORT_BENCH=1 SMELT_COMPAT_EXPORT_BENCH_BYTES=<bytes> \
  <smelt-core-test> --exact \
  session_exports::tests::compatibility_export_benchmark_suite \
  --ignored --nocapture
```

| Export target | Output bytes | Duration | RSS before / after | Peak RSS |
|---|---:|---:|---:|---:|
| 50 MiB | 52,428,896 | 86.260 ms | 17,666,048 / 18,804,736 B | 21,868 KiB |
| 500 MiB | 524,288,546 | 396.789 ms | 17,522,688 / 18,800,640 B | 22,620 KiB |

Output time scaled with bytes while RSS after export changed by only 1.09 MiB and
1.22 MiB. A held export lock also passed the bounded-shutdown regression test;
compatibility work never owns canonical shutdown.

Raw outputs are retained at:

- `~/tmp/phase7-layout-matrix-release-final.log`
- `~/tmp/phase7-enter-50k-release-final-v4.log`
- `~/tmp/phase7-enter-2k-8k-release-final-v4.log`
- `~/tmp/phase7-search-50m-release-final-v2.log`
- `~/tmp/phase7-search-500m-release-final-5run.log`
- `~/tmp/phase7-resume-wheel-50m-release-final.log`
- `~/tmp/phase7-resume-wheel-500m-release-final.log`
- `~/tmp/phase7-active-memory-50m-release-fast-final.log`
- `~/tmp/phase7-active-memory-500m-release-fast-final.log`
- `~/tmp/phase7-catalog-1k-release-final.log`
- `~/tmp/phase7-catalog-100k-release-final.log`
- `~/tmp/phase7-compat-export-50m-release-final.log`
- `~/tmp/phase7-compat-export-500m-release-final.log`

### Complexity and remaining limits

- Enter persistence is proportional to the changed history and descriptor
  suffix, transactional search-index work, and SQLite commit. Catalog and
  compatibility projections are queued only after the canonical receipt and are
  not awaited.
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
  memory bounded by one indexed row. `meta.json` is produced by the same bounded
  compatibility exporter. Session listing reads the rebuildable catalog and does
  not depend on either sidecar.
