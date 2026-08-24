# Transcript layout benchmarks

Run the transcript layout benchmark suite through xtask:

```bash
cargo xtask bench-transcript-layout --runs 5
```

The command runs the feature-gated `smelt-tui` transcript benchmark target in
release mode and prints:

- one sample line per workload/run;
- a mean±standard-deviation table for projection/cache workloads;
- structural counters for layout compilation, exact height measurement, and
  visible materialization;
- app-level navigation/search timings for `/`, Ctrl-D, Ctrl-U, `gg`, and `G`
  over a warmed transcript unless `--skip-nav` is passed;
- real mouse-click and Lua callback timings for the previous-user and bottom
  scroll pills;
- collapsed, expansion, and top/middle/deep wheel-scroll timings for a real
  expanded `write_file` source view.

Useful variants:

```bash
# Run only the 10 MiB mixed workload.
cargo xtask bench-transcript-layout --runs 10 --workloads mixed_10mib

# Run the 50 MiB mixed workload without the navigation suite.
cargo xtask bench-transcript-layout --runs 1 --workloads mixed_50mib --skip-nav

# Run a comma-separated subset.
cargo xtask bench-transcript-layout --runs 5 --workloads markdown_4mib,tool_output_4mib

# Run only the tall expanded write_file interaction with 20,000 source lines.
cargo xtask bench-transcript-layout --runs 5 --tall-write-only \
  --tall-write-lines 20000

# Render one retained edit diff over 20,000-line before/after files.
cargo xtask bench-transcript-layout --runs 5 --tall-diff-only \
  --tall-diff-lines 20000

# Debug/test profile for instrumentation work; do not use for timing baselines.
cargo xtask bench-transcript-layout --debug --runs 1 --workloads mixed_10mib
```

## Provider, tool, and local command streaming

The streaming suite sends realistic output through the application harness. Chunk
sizes vary deterministically, and payloads mix Markdown, code fences, ANSI color,
UTF-8, status lines, warnings, and line boundaries. Setup and the initial
transcript render are outside the measured interval. Select one of six workloads:

- `text`: provider `TextDelta` events with streaming Markdown;
- `bash`: provider `ToolStarted`, `ToolOutput`, and `ToolFinished` events for a
  `bash` tool;
- `mixed`: reasoning, text, and provider `bash` phases in one turn;
- `exec`: the local `!command` transcript sink's start, append, and finish path;
- `write-draft`: streamed `write_file` JSON arguments, including the incremental
  draft preview and final replacement;
- `explore-group`: interleaved output from multiple grouped explore tools, with
  `--stream-parallel-tools` controlling child count.

The default mode forces a full silent terminal redraw after every event. It is a
diagnostic upper bound for revision and layout scaling. `--stream-scheduled`
advances the harness clock in deterministic bursts and uses the production frame
scheduler without user interaction. `--stream-scroll` uses the same scheduled mode
and adds urgent transcript navigation.

```bash
# Reproduce provider bash output in a small session.
cargo xtask bench-transcript-layout --runs 5 --stream-only \
  --stream-workload bash --stream-scheduled --stream-history 5 \
  --stream-chunks 512 --stream-bytes 65536

# Exercise reasoning, Markdown, and tool lifecycle transitions together.
cargo xtask bench-transcript-layout --runs 5 --stream-only \
  --stream-workload mixed --stream-scheduled --stream-history 20 \
  --stream-chunks 512 --stream-bytes 65536

# Exercise the local shell-escape transcript append path.
cargo xtask bench-transcript-layout --runs 5 --stream-only \
  --stream-workload exec --stream-scheduled --stream-history 5 \
  --stream-chunks 512 --stream-bytes 65536

# Stream a large write_file draft through incremental JSON preview parsing.
cargo xtask bench-transcript-layout --runs 1 --stream-only \
  --stream-workload write-draft --stream-scheduled \
  --stream-chunks 128 --stream-bytes 196608

# Interleave eight grouped explore tools with 1 MiB of output each.
cargo xtask bench-transcript-layout --runs 1 --stream-only \
  --stream-workload explore-group --stream-scheduled \
  --stream-parallel-tools 8 --stream-chunks 16 --stream-bytes 1048576

# Continue autonomous frames after a sparse 5 MiB tail stops streaming.
cargo xtask bench-transcript-layout --runs 1 --stream-only \
  --stream-workload text --stream-scheduled --stream-resumed-bytes 5242880 \
  --stream-resumed-position tail --stream-chunks 1 --stream-bytes 1 \
  --stream-idle-frames 100

# Stress persisted extent boundaries with large records at top, middle, and tail
# of a 600-record fixture.
cargo xtask bench-transcript-layout --runs 1 --stream-only \
  --stream-workload text --stream-scheduled --stream-resumed-bytes 1 \
  --stream-resumed-position tail --stream-boundary-record-bytes 524288 \
  --stream-chunks 1 --stream-bytes 1 --stream-idle-frames 20

# Detect work proportional to prior transcript size while navigating.
cargo xtask bench-transcript-layout --runs 5 --stream-only --stream-scroll \
  --stream-workload text --stream-history 10000 \
  --stream-chunks 512 --stream-bytes 16384
```

Each run reports event-kind dispatch tails, total and compositor frame tails,
actual traced frames, request-to-terminal-flush p99 latency, calling-thread
allocations, and process allocation, deallocation, and retained bytes. Targeted
perf rows attribute tool append, draft parsing, retained output registration,
group revision updates, Lua layout compilation, capped text layout, sparse
planning and hydration, persisted extent reconstruction, semantic navigation,
committed-view callbacks, transcript projection, reader opens, and terminal flush
work.
Use response-byte scaling to detect work proportional to accumulated live output,
chunk-count scaling to detect per-event amplification, and history scaling to
detect global scene or cache scans.

The rearchitecture acceptance gates are:

- input-to-frame below 16.7 ms p95 and 33 ms p99 while streaming;
- streaming document and scene work independent of prior transcript block count;
- a 64 KiB response in 2,048 chunks allocates less than 16 MiB in the warmed
  document and scene hot path;
- ordinary streaming frames are capped at 60 per second while first output, final
  output, and interaction frames remain urgent.

A pre-fix release reference at `6101512a4`, using five prior blocks, 512 chunks,
and 64 KiB of payload, reproduced the small-session spike:

| Scheduled workload | Frames | Total | Allocated | Frame p99 |
|---|---:|---:|---:|---:|
| Text | 40 | 40-48 ms | 37.4-37.6 MB | 2.13-2.76 ms |
| Provider bash | 515 | 3.80-3.85 s | 3.99 GB | 14.58-14.80 ms |
| Mixed | 285 | 1.05-1.11 s | 1.11 GB | 7.60-8.00 ms |
| Local exec | 34 | 255-264 ms | 313 MB | 13.37-13.84 ms |

The provider bash case rendered once per event because `ToolOutput` was urgent.
A temporary controlled change that treated only continued `ToolOutput` as
coalescible reduced it from 515 to 35 frames, from about 3.8 seconds to 252 ms,
and from 3.99 GB to 275 MB allocated. The remaining frame p99 stayed near 14.5 ms
because capped ANSI output still measured and projected the accumulated output.

A separate temporary change batched adjacent retained cap rows into one child
range render. With frame scheduling unchanged at 515 frames, it reduced total
time from 3.93 to 1.15 seconds, frame p99 from 15.11 to 4.41 ms, and allocation
from 3.99 to 1.08 GB. Both controlled changes were reverted after measurement.
Prior-history scaling from 5 to 100 blocks was flat, while active-output byte and
chunk-count scaling were not.

### Phase 1 canonical content and retained compositor result

The Phase 1 release benchmark uses five prior blocks, 2,048 output events, and a
64 KiB final payload. Provider and local exec output now transfer each incoming
line into the same stable chunked content channel. Appends emit typed byte-range
patches, keep stable content IDs, and do not replace accumulated output. Retained
renderer callbacks, root layout composition, and transcript row tapes rerun only
when their semantic inputs change.

```bash
cargo xtask bench-transcript-layout --runs 1 --stream-only \
  --stream-workload bash --stream-scheduled --stream-history 5 \
  --stream-chunks 2048 --stream-bytes 65536
cargo xtask bench-transcript-layout --runs 1 --stream-only \
  --stream-workload exec --stream-scheduled --stream-history 5 \
  --stream-chunks 2048 --stream-bytes 65536
```

| Workload | Frames | Total | Frame p95 | Frame p99 | Allocated |
|---|---:|---:|---:|---:|---:|
| Provider bash | 131 | 29.816 ms | 0.313 ms | 0.386 ms | 14,940,677 bytes |
| Local exec | 130 | 15.874 ms | 0.142 ms | 0.282 ms | 10,810,903 bytes |

The provider workload is 1,836,539 bytes below the 16 MiB allocation gate. Its
130 actual transcript projections perform 520 retained row-index preparations,
28 structural block compilations, and no projection on the final unchanged
frame.

Animation isolation was measured by appending 120 scheduled spinner-only frames
to the same provider workload:

```bash
cargo xtask bench-transcript-layout --runs 1 --stream-only \
  --stream-workload bash --stream-scheduled --stream-history 5 \
  --stream-chunks 2048 --stream-bytes 65536 --stream-idle-frames 120
```

| Metric | No idle frames | 120 idle frames |
|---|---:|---:|
| Compositor frames | 131 | 251 |
| Transcript projection | 130 | 130 |
| Viewport and sparse plans | 130 | 130 |
| Hydration plans | 130 | 130 |
| Row-index preparations | 520 | 520 |
| Visible-range projections | 130 | 130 |
| Structural layout compilations | 130 | 130 |
| Full block metadata compilations | 12 | 12 |

The idle segment therefore performs zero transcript projection, sparse planning,
hydration planning, row-index preparation, structural Lua compilation, payload
hashing, or storage reads. The retained prompt top bar still repaints each visible
spinner frame.

### Phase 2 incremental drafts, groups, and retained files

The Phase 2 release workloads use 100 prior blocks. The draft workload streams a
1 MiB top-level `write_file` string in 128 chunks. The grouped workload interleaves
16 chunks of 1 MiB across each of eight children, for 8 MiB of active child output.

```bash
cargo xtask bench-transcript-layout --runs 1 --stream-only \
  --stream-workload write-draft --stream-scheduled --stream-history 100 \
  --stream-chunks 128 --stream-bytes 1048576
cargo xtask bench-transcript-layout --runs 1 --stream-only \
  --stream-workload explore-group --stream-scheduled --stream-history 100 \
  --stream-parallel-tools 8 --stream-chunks 16 --stream-bytes 1048576
cargo xtask bench-transcript-layout --runs 1 --tall-write-only \
  --tall-write-lines 20000
```

| Scheduled workload | Events | Frames | Total | Dispatch p99 | Frame p99 | Thread allocation |
|---|---:|---:|---:|---:|---:|---:|
| 1 MiB `write_file` draft | 131 | 11 | 26.082 ms | 0.885 ms | 0.754 ms | 5,985,065 bytes |
| Eight-child explore group | 145 | 26 | 31.098 ms | 1.459 ms | 0.863 ms | 7,864,979 bytes |

The incremental draft parser itself performed 128 appends with 461 allocations
and 1,680,433 allocated bytes. Its p95 allocation was 43,952 bytes, its maximum
was 306,096 bytes, and its maximum append span was 0.923 ms. Finalization reuses
the parser and retained field content. An exact final value performs no replay;
a longer value appends only its suffix, and mismatch reconciliation preserves the
stable content identity.

Group membership and structure keys now contain stable child identities,
revisions, and bounded typed metadata rather than child payload snapshots.
Ordinary child appends do not promote the parent presentation revision or rerender
unchanged siblings. Planning a stored group uses payload-independent metadata from
the stored record references and leaves child payloads unhydrated. Across all 128
child appends, the append hot path allocated 1,236,152 bytes, with a maximum of
65,577 bytes for one append.

The retained file workload contains 1,448,889 bytes and 20,000 source lines. Its
final release sample was:

| Retained `write_file` phase | Time |
|---|---:|
| Collapsed 12-frame scroll | 1.336 ms |
| Enter and expansion | 2.284 ms |
| Top 12-frame scroll | 1.807 ms |
| Middle 12-frame scroll | 1.603 ms |
| Deep 12-frame scroll | 1.204 ms |

Logical line boundaries remain canonical in shared transcript content, while each
width-specific file layout stores only sparse wrap boundaries. Rendering rebuilds
requested rows from those two bounded indexes instead of retaining full row
structures or rescanning from byte zero.

The Phase 2 correctness matrix passed 5,063 workspace tests with 2 skipped.
Workspace clippy, formatting, the `release-fast` smelt build, transcript storybook
review, and Lua API generation all passed. Line coverage was 86.15 percent, no
snapshot updates remained, and `git diff --check` was clean.

### Phase 3 retained renderer and Lua contract

Phase 3 replaced renderer payload snapshots with typed `TranscriptRenderNode`
metadata and direct Rust-to-Lua table construction. Large completed and promoted
preview fields for `edit_file`, `edit_notebook`, and `present_plan` now cross the
tool lifecycle as shared retained content, outside bounded JSON metadata. Lua
receives content IDs, revisions, sizes, and bounded previews. It resolves source
only through retained content layouts. The superseded source-view mirror cache,
its memory budgets, and its public layout leaf were deleted.

The 64 KiB provider gate remained below the 16 MiB allocation limit after the
contract replacement:

| Provider bash metric | Phase 3 result |
|---|---:|
| Events / compositor frames | 2,051 / 131 |
| Total | 30.278 ms |
| Frame p95 / p99 / maximum | 0.293 / 0.415 / 2.178 ms |
| Thread allocation | 14,813,116 bytes |
| Process retained bytes | 1,288,938 bytes |

This is 1,964,100 bytes below the allocation gate. The release regressions for
incremental drafts, retained groups, and retained file views also remained flat:

| Scheduled workload | Events | Frames | Total | Dispatch p99 | Frame p99 | Thread allocation |
|---|---:|---:|---:|---:|---:|---:|
| 1 MiB `write_file` draft | 131 | 11 | 25.598 ms | 0.963 ms | 0.722 ms | 5,955,938 bytes |
| Eight-child explore group | 145 | 26 | 27.447 ms | 1.299 ms | 0.896 ms | 7,921,854 bytes |

The draft parser still used 461 allocations and 1,680,433 bytes across 128
appends, with 43,952-byte p95 and 306,096-byte maximum append allocation. The
group child-append path used 516 allocations and 1,236,344 bytes across 128
appends, with a 65,577-byte p95 and maximum. On the 1,448,889-byte, 20,000-line
retained `write_file`, collapsed scrolling took 1.461 ms, expansion took 2.514 ms,
and 12-frame top, middle, and deep scrolling took 1.310, 1.249, and 1.230 ms.

The Phase 3 correctness matrix passed 5,066 workspace tests with 2 skipped.
Strict workspace clippy, formatting, Lua API generation, focused edit, notebook,
plan, and retained-diff tests, reviewed transcript storybooks, coverage, and the
`release-fast` smelt build passed. Line coverage was 86.12 percent, no generated
snapshot updates remained, and `git diff --check` was clean. The release artifacts
are `/tmp/smelt-phase3-provider-bash.txt`,
`/tmp/smelt-phase3-write-draft.txt`, `/tmp/smelt-phase3-explore-group.txt`,
`/tmp/smelt-phase3-tall-write.txt`, `/tmp/smelt-phase3-retained-diff-20k.txt`,
and `/tmp/smelt-phase3-retained-diff-80k.txt`.

### Phase 4 bounded row indexes and caps

A dedicated release benchmark measures completed `edit_file` rendering through
the retained content-diff path. `source_bytes` is the combined size of the old
and new files. The initial render includes Lua metadata compilation and retained
diff IR construction. The warm render has no semantic changes.

```bash
cargo xtask bench-transcript-layout --runs 1 --tall-diff-only \
  --tall-diff-lines 20000 --no-warmup
cargo xtask bench-transcript-layout --runs 1 --tall-diff-only \
  --tall-diff-lines 80000 --no-warmup
```

| Retained diff metric | 20,000 lines | 80,000 lines |
|---|---:|---:|
| Combined source bytes | 2,777,792 | 11,177,792 |
| First render | 6.438 ms | 5.682 ms |
| Warm render | 0.084 ms | 0.072 ms |
| First-render allocations | 12,651 / 3,948,287 bytes | 12,652 / 3,948,438 bytes |
| Warm-render allocations | 339 / 327,911 bytes | 339 / 327,914 bytes |
| Process retained bytes | 3,355,868 | 3,355,874 |
| Canonical live tool state | 2,818,186 bytes | 11,338,186 bytes |
| Retained layout | 22,063 bytes | 22,066 bytes |
| Height index / visible rows | 11,888 / 4,624 bytes | 11,888 / 4,624 bytes |
| Full retained rows | 0 bytes | 0 bytes |
| Metadata compiles, first / warm | 26 / 0 | 26 / 0 |

The first compilation now compares retained chunks directly, narrows work by the
common byte prefix and suffix, expands the changed interval to complete lines and
three context rows, and tokenizes only that bounded window. It neither snapshots
the complete sources nor builds complete old/new line vectors. First-render
allocation differs by only 151 bytes between the 20,000-line and 80,000-line
fixtures. Retained layout, height-index, visible-row, and post-render process
memory also stay flat while canonical source ownership scales as expected.

For comparison, the Phase 3 complete-source implementation took 13.579 ms and
allocated 17,414,681 bytes at 20,000 lines, then took 46.201 ms and allocated
57,093,680 bytes at 80,000 lines. An intermediate snapshot-free implementation
that still built complete line vectors allocated 11,837,868 and 35,509,443 bytes.
The narrowed retained implementation removes both scaling terms. An unchanged
warm render remains payload-independent at about 0.07 ms, 328 KB allocated, and
zero metadata recompilations.

Retained viewport rendering now always carries a `MeasuredLayout` through Vboxes,
Hboxes, panels, prefixes, gutters, and style wrappers. Missing child measurements
are structural errors rather than a reason to measure the complete child during
a row query. Complete rendering remains an explicit mode for one-off previews,
while caps use retained edge queries or transient layouts guarded by byte, node,
span, and recursion budgets.

The completed Phase 4 provider matrix uses the production semantic scheduler and
retained transcript projection:

```bash
cargo xtask bench-transcript-layout --runs 1 --stream-only \
  --stream-workload bash --stream-scheduled --stream-history 5 \
  --stream-chunks 2048 --stream-bytes 65536
cargo xtask bench-transcript-layout --runs 1 --stream-only \
  --stream-workload bash --stream-scheduled --stream-history 5 \
  --stream-chunks 128 --stream-bytes 8388608 --no-warmup
cargo xtask bench-transcript-layout --runs 1 --stream-only \
  --stream-workload bash --stream-scheduled --stream-history 5 \
  --stream-chunks 1 --stream-bytes 100663296 --no-warmup
```

| Provider bash workload | Events | Frames | Total | Dispatch p99 | Frame p95 / p99 / max | Thread allocation |
|---|---:|---:|---:|---:|---:|---:|
| Warmed 64 KiB / 2,048 chunks | 2,051 | 131 | 31.969 ms | 0.009 ms | 0.260 / 0.390 / 2.540 ms | 16,273,090 bytes |
| 8 MiB / 128 chunks | 131 | 11 | 30.284 ms | 1.095 ms | 2.107 / 2.107 / 2.107 ms | 5,708,304 bytes |
| 96 MiB / one chunk | 4 | 27 | 312.274 ms | 11.140 ms | 14.800 / 15.357 / 15.357 ms | 31,772,080 bytes |

The warmed 64 KiB run is 504,126 bytes below the 16 MiB allocation gate. Its 130
transcript projections reuse visible block-layout and row-identity capacities.
Text cap rendering retains at most 64 tail rows per width and ANSI mode,
invalidates only the changed logical-line suffix, keeps cap selection metadata
on the stack, and directly renders non-truncated caps. Append mutation itself
used 228,548 bytes across 1,024 measured append spans.

A provider chunk is transferred once into a shared `Arc<String>`. The first 4 MiB
slice is applied immediately and each remaining UTF-8-safe ranged slice advances
at one semantic render boundary, which produced 27 traced frames for the 96 MiB
sample. Completion cannot overtake those slices. History replacement and terminal
turn events use the same ordering boundary, while cancellation and transcript
clearing discard queued output and deferred lifecycle events. The final provider
completion reuses the already-streamed retained content identity and clears its
redundant complete payload before queueing.

Compared with the investigation reference below, the 8 MiB frame p99 fell from
153.37 to 2.107 ms and total allocation fell from 7.85 GB to 5.71 MB. The 96 MiB
worst frame fell from 2.088 seconds with a 981 MB worst-frame allocation to
15.357 ms; total measured allocation is 31.77 MB. Fixed small-output streaming no
longer follows `O(events * accumulated_output)`: the 2,048-chunk 64 KiB workload
completes under the 16 MiB allocation gate.

The final 96 MiB and 8 MiB logs are
`/tmp/smelt-phase4-final-retained-bash-96mib.txt` and
`/tmp/smelt-phase4-final-retained-bash-8mib.txt`.

The final Phase 4 correctness matrix passed 5,126 workspace tests with 2 skipped.
Strict workspace clippy, formatting, Lua API generation, the `release-fast` smelt
build, and `git diff --check` passed. Line coverage was 86.33 percent against the
80 percent gate, and no generated `.snap.new` files remained.

### Phase 5 semantic scheduler and compositor

Phase 5 routes production provider and local exec continuations through one typed
frame-boundary queue. Provider text, reasoning, tool draft, tool output, and
`EngineAskDelta` events join the queue after their first visible mutation. Local
exec append, finish, and finalize use the same queue after urgent block creation.
The queue is applied exactly once at compositor frame start. Urgent completion and
error events apply queued predecessors first, while cancellation and transcript
reset discard pending mutations and deferred lifecycle work together.

The benchmark records one `transcript:pending_work:applied` value per frame and
fails if its sample count differs from traced compositor frames. The
production benchmark-only exec scheduling override was deleted, so these samples
exercise producer-owned urgency and the same scheduler as the application.

```bash
cargo xtask bench-transcript-layout --runs 1 --stream-only \
  --stream-workload bash --stream-scheduled --stream-history 5 \
  --stream-chunks 2048 --stream-bytes 65536
cargo xtask bench-transcript-layout --runs 1 --stream-only \
  --stream-workload exec --stream-scheduled --stream-history 5 \
  --stream-chunks 2048 --stream-bytes 65536
cargo xtask bench-transcript-layout --runs 1 --stream-only \
  --stream-workload bash --stream-scheduled --stream-history 5 \
  --stream-chunks 2048 --stream-bytes 65536 --stream-idle-frames 120
cargo xtask bench-transcript-layout --runs 1 --stream-only \
  --stream-workload bash --stream-scheduled --stream-history 5 \
  --stream-chunks 128 --stream-bytes 8388608 --no-warmup
cargo xtask bench-transcript-layout --runs 1 --stream-only \
  --stream-workload bash --stream-scheduled --stream-history 5 \
  --stream-chunks 1 --stream-bytes 100663296 --no-warmup
```

| Scheduled workload | Events | Frames | Total | Frame p95 / p99 / max | Thread allocation |
|---|---:|---:|---:|---:|---:|
| Provider, 64 KiB / 2,048 chunks | 2,051 | 131 | 33.762 ms | 0.378 / 0.430 / 2.199 ms | 16,321,104 bytes |
| Local exec, 64 KiB / 2,048 chunks | 2,050 | 130 | 20.960 ms | 0.219 / 0.343 / 0.568 ms | 10,674,526 bytes |
| Provider plus 120 idle frames | 2,051 | 251 | 35.832 ms | 0.277 / 0.305 / 2.063 ms | 19,792,682 bytes |
| Provider, 8 MiB / 128 chunks | 131 | 11 | 32.122 ms | 4.663 / 4.663 / 4.663 ms | 5,755,422 bytes |
| Provider, 96 MiB / one chunk | 4 | 28 | 292.179 ms | 13.158 / 15.528 / 15.528 ms | 31,878,227 bytes |

The final narrow typed-queue 64 KiB provider sample is 456,112 bytes below the
16 MiB allocation gate. Its queue metric has 131 samples and 2,048 total applied
mutations; local exec has
130 samples and 2,050 applied mutations. The 8 MiB and 96 MiB runs likewise have
one queue sample per frame. Provider completion remains ordered after the 4 MiB
bounded ingestion slices, so the 96 MiB event advances over 28 frames and never
approaches the 33 ms maximum-frame gate.

The 120 added spinner frames did not add transcript work. Both the ordinary and
idle-extended provider runs performed 130 transcript projections. Viewport plans,
sparse plans, and transcript scene refreshes also remained at 130 while compositor
frames rose from 131 to 251. Animation therefore reuses retained transcript state.
Terminal paint then follows the existing `Ui::flush_prepared_frame` to
`Compositor::flush_frame`, `Grid::diff`, and `flush_diff` chain, which emits only
changed cells.

The first prior-history comparison failed the scaling gate and exposed an
unbounded invalidation path: each timed tool-renderer refresh scanned every node in
the complete active height index. Timed refresh now resolves dirty nodes through
the retained scene index and clears complete width snapshots as a bounded set. The
same three-run release workload was then repeated with 5 and 10,000 prior blocks:

```bash
cargo xtask bench-transcript-layout --runs 3 --stream-only \
  --stream-workload bash --stream-scheduled --stream-history 5 \
  --stream-chunks 2048 --stream-bytes 65536
cargo xtask bench-transcript-layout --runs 3 --stream-only \
  --stream-workload bash --stream-scheduled --stream-history 10000 \
  --stream-chunks 2048 --stream-bytes 65536
```

| Prior blocks | Warmed mean frame | Three-run mean frame p99 | Three-run total mean |
|---:|---:|---:|---:|
| 5 | 0.233 ms | 0.497 ms | 32.212 ms |
| 10,000 | 0.222 ms | 0.388 ms | 32.037 ms |

The warmed frame cost decreased by 4.7 percent rather than growing with history,
passing the 10 percent gate. The final artifacts are
`/tmp/smelt-phase5-bash-64k-typed-final.txt`,
`/tmp/smelt-phase5-history-5-fixed-final.txt`,
`/tmp/smelt-phase5-history-10000-fixed-final.txt`,
`/tmp/smelt-phase5-exec-64k-final.txt`,
`/tmp/smelt-phase5-animation-final.txt`,
`/tmp/smelt-phase5-bash-8m-final.txt`, and
`/tmp/smelt-phase5-bash-96m-final.txt`.

The final Phase 5 correctness matrix passed 5,132 workspace tests with 2 skipped.
Strict workspace clippy, formatting, Lua API generation, the `release-fast` smelt
build, snapshot review, and `git diff --check` passed. Line coverage was 86.34
percent against the 80 percent gate, and no generated `.snap.new` files remained.

### Phase 6 sparse indexes and persistent hydration

Phase 6 replaces root-scoped 64-record extent chunks with immutable payload
profiles and persistent sequence-node aggregates. Extent range and total, row
lookup, semantic kind/role navigation, and block lookup descend the canonical
content-addressed transcript sequence. The profile path remains usable when every
transcript object byte is corrupt, proving that planning and navigation do not
hydrate payloads. Schema version 1 databases migrate transactionally to version 2;
a two-record rollback fixture corrupts the second payload so the test verifies
rollback after successful partial backfill.

The release scaling matrix creates valid persistent sequence trees directly in
SQLite, calls production reader APIs 1,001 times per size, and asserts zero
`store:object:payloads_loaded`:

```bash
set -o pipefail
SMELT_SPARSE_INDEX_BENCH=1 \
SMELT_SPARSE_INDEX_BENCH_RUNS=1001 \
cargo test --release -p smelt-store \
  sparse_index_scaling_benchmark_suite \
  -- --ignored --nocapture 2>&1 | tail -240
```

| Records | Fixture | Previous kind p99 | Next role p99 | Block lookup p99 | Row lookup p99 | Extent range p99 | Extent total p99 | Payloads loaded |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 10,000 | 144 ms | 204 us | 200 us | 465 us | 441 us | 446 us | 13 us | 0 |
| 100,000 | 1,460 ms | 311 us | 280 us | 703 us | 705 us | 592 us | 12 us | 0 |
| 1,000,000 | 15,011 ms | 308 us | 242 us | 777 us | 708 us | 502 us | 10 us | 0 |

Every semantic navigation result is below the 2 ms p99 gate. The row and block
lookups are also below 1 ms p99 at all three sizes, and query latency does not grow
linearly with record count.

The active transcript document retains the metadata reader opened for the initial
resume tail. Viewport planning only queues missing ranges. A bounded post-dispatch
drain merges adjacent ranges, and a background worker hydrates them through its
own persistent payload reader before publishing the result on the next redraw.
Sparse prefix, scrollbar total, and row lookup share one retained width/root extent
result. Dynamic prefix and row-location lookups each have a 256-entry eviction
limit.

The committed-view watcher benchmark scrolls a resumed 5,000-block transcript for
1,001 frames with a retained Lua watcher. It fails above 2 ms p99, on a reader
replacement, or on any additional store-open attempt:

```bash
set -o pipefail
SMELT_TRANSCRIPT_BENCH_TARGET=1 \
SMELT_TRANSCRIPT_SPARSE_WATCHER_BENCH=1 \
cargo test --release -p smelt-tui \
  --features harness,transcript-bench \
  transcript_sparse_watcher_benchmark_suite \
  -- --nocapture 2>&1 | tail -240
```

| Watcher blocks | Dispatches | p50 | p95 | p99 | Maximum | Metadata / hydration readers | Additional opens |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 5,000 | 1,001 | 333 us | 445 us | 524 us | 1,739 us | 1 / 1 | 0 |

This replaces the investigation reference's 142.0 ms dispatch, 534 MB callback
allocation, and up-to-256-record payload scan. The new p99 is more than 270 times
lower and performs metadata-only dispatch.

The autonomous comparison builds and saves one deterministic 5 MiB transcript,
measures 600 warmed frames while it is fully hydrated, resumes the same persisted
content sparsely, and measures the equivalent active text turn. Fixture creation,
resume, first output, and 32 warmup frames are outside each measured interval. The
test uses the production scheduler, silent terminal path, process and thread
allocation counters, and fails when sparse compositor time or allocation exceeds
the hydrated result by 25 percent:

```bash
set -o pipefail
SMELT_TRANSCRIPT_BENCH_TARGET=1 \
SMELT_TRANSCRIPT_SPARSE_AUTONOMOUS_BENCH=1 \
cargo test --release -p smelt-tui \
  --features harness,transcript-bench \
  transcript_sparse_autonomous_frame_benchmark_suite \
  -- --nocapture 2>&1 | tail -240
```

| Representation | Compositor total | Frame p50 / p95 / p99 / max | Thread allocations | Thread bytes | Process allocation |
|---|---:|---:|---:|---:|---:|
| Hydrated | 40,958 us | 53 / 145 / 205 / 251 us | 240,272 | 22,937,298 | 22,937,298 bytes |
| Sparse | 37,724 us | 50 / 120 / 171 / 1,001 us | 241,471 | 23,337,677 | 23,337,677 bytes |
| Sparse / hydrated | 0.921 | - | 1.005 | 1.017 | 1.017 |

The sparse interval loads zero transcript payloads, keeps exactly one metadata
reader and one hydration reader, and makes zero additional open attempts. Compared
with the old 600-frame
reference below, sparse total time fell from 1.573 seconds to 37.7 ms and
allocation fell from 1.220 GB to 23.3 MB. The previous sparse path was 16.1 times
slower and allocated 25.7 times more than hydrated; the retained profile path is
now faster by compositor total and within 1.8 percent by allocated bytes.

A resumed provider stream uses the same `transcript_stream_benchmark_suite`. Its
output reports `metadata_readers`, `hydration_readers`, `total_readers`, and the
corresponding metadata, hydration, and total open-attempt counts. The benchmark
captures all six after warmup and asserts that none changes across any provider
event, scheduled frame, or idle frame.

```bash
set -o pipefail
SMELT_TRANSCRIPT_BENCH_TARGET=1 \
SMELT_TRANSCRIPT_STREAM_BENCH=1 \
SMELT_TRANSCRIPT_STREAM_WORKLOAD=text \
SMELT_TRANSCRIPT_STREAM_SCHEDULED=1 \
SMELT_TRANSCRIPT_STREAM_RESUMED_BYTES=5242880 \
SMELT_TRANSCRIPT_STREAM_RESUMED_POSITION=tail \
SMELT_TRANSCRIPT_STREAM_CHUNKS=2048 \
SMELT_TRANSCRIPT_STREAM_BYTES=65536 \
SMELT_TRANSCRIPT_STREAM_IDLE_FRAMES=120 \
SMELT_TRANSCRIPT_BENCH_RUNS=1 \
cargo test --release -p smelt-tui \
  --features harness,transcript-bench \
  transcript_stream_benchmark_suite -- --nocapture
```

The release sample applied 2,049 provider events over 252 scheduled frames,
including 120 idle frames. It retained one metadata reader and one hydration
reader, with zero additional open attempts during the measured interval. Both
reader counts were therefore independent of event and frame count. Frame
p95/p99/maximum were 1.611/1.839/4.443 ms.

### Phase 7 final acceptance matrix

The final reflection pass replaced compositor-path hydration with a persistent
per-session background worker, made the harness settle through that production
worker, consolidated autonomous transcript work into one insertion-ordered indexed
queue, and split transcript and retained-layout internals by ownership. The
benchmark-only feature combination is covered by an explicit strict clippy gate in
addition to workspace validation.

The post-reflection release provider rerun used the production scheduler,
background hydration service, unified ordered work queue, and retained renderer:

| Provider bash workload | Events | Frames | Total | Frame p95 / p99 / max | Thread allocation |
|---|---:|---:|---:|---:|---:|
| Warmed 64 KiB / 2,048 chunks | 2,051 | 131 | 32.805 ms | 0.376 / 0.414 / 2.048 ms | 16,325,277 bytes |
| 8 MiB / 128 chunks | 131 | 11 | 29.459 ms | 3.406 / 3.406 / 3.406 ms | 5,754,773 bytes |
| 96 MiB / one chunk | 4 | 29 | 275.207 ms | 11.658 / 11.889 / 11.889 ms | 31,951,620 bytes |

The warmed 64 KiB sample is 451,939 bytes below the 16 MiB allocation gate. The
unified `TranscriptWorkQueue` was applied once in each traced frame and consumed
all continuation and lifecycle work in insertion order. The 8 MiB and 96 MiB
samples remain below both frame gates. The 96 MiB input advances in UTF-8-safe
4 MiB slices rather than making one frame absorb the provider event.

The equivalent post-reflection local exec rerun consumed all 2,050 lifecycle and
output mutations in 130 frame-boundary queue passes. It completed in 20.029 ms,
allocated 10,679,768 thread bytes, and recorded 0.216/0.319/0.514 ms frame
p95/p99/maximum. Provider and local exec therefore retain the same bounded append
and scheduling shape.

The remaining streaming and retained-layout gates were rerun from the same release
binary:

| Final Phase 7 workload | Total | Frame p95 / p99 / max | Thread allocation |
|---|---:|---:|---:|
| 1 MiB `write_file` draft | 27.674 ms | - / 3.485 / - ms | 6,550,163 bytes |
| Eight-child, 1 MiB-per-child group | 30.473 ms | 4.671 / 6.837 / 6.837 ms | 7,249,863 bytes |
| Resumed 5 MiB tail plus 2,048 provider chunks and 120 idle frames | 171.024 ms | 1.537 / 2.106 / 3.328 ms | 136,277,432 bytes |

The draft parser's 128 incremental appends allocated 2,253,533 bytes, below the
three-times-input gate, and its p95/p99/maximum spans were 0.894/0.994/1.023 ms.
Finalization reused retained typed fields. The group child-append path allocated
1,775,112 bytes across 128 appends, with 98,457 bytes maximum for one append. The
full grouped frame remained below 16 MiB and both frame latency gates.

The resumed provider sample used 252 frames, retained exactly one metadata reader
and one hydration reader, and made zero additional open attempts across streaming
and idle work. The animation-only
comparison used 251 frames, completed in 36.229 ms, and recorded 0.400 ms frame
p99. It performed transcript projection, viewport and sparse planning, hydration
planning, and retained scene work only 130 times, the same as the no-idle stream.

The final three-run history comparison remained independent of prior block count:

| Prior blocks | Mean frame | Mean frame p99 | Total mean |
|---:|---:|---:|---:|
| 5 | 0.218 ms | 0.426 ms | 29.586 ms |
| 10,000 | 0.238 ms | 0.427 ms | 32.301 ms |

Mean frame cost increased about 9.2 percent, below the 10 percent scaling gate,
while mean p99 remained effectively flat. The retained-diff rerun allocated
3,985,838 bytes for 20,000 lines and 3,985,989 bytes for 80,000 lines, a difference
of 151 bytes. First renders took 4.942/5.012 ms, warm renders took 0.070/0.071 ms,
retained layout stayed at 22,063/22,066 bytes, height and visible-row indexes stayed
fixed, and full retained rows remained zero.

The post-reflection sparse-index matrix again loaded zero payloads:

| Records | Previous kind p99 | Next role p99 | Block lookup p99 | Row lookup p99 | Extent range p99 | Extent total p99 |
|---:|---:|---:|---:|---:|---:|---:|
| 10,000 | 173 us | 169 us | 424 us | 416 us | 368 us | 11 us |
| 100,000 | 244 us | 219 us | 573 us | 550 us | 495 us | 10 us |
| 1,000,000 | 277 us | 220 us | 747 us | 682 us | 467 us | 9 us |

The watcher published 1,024 committed-view samples for 1,001 actions because
production-identical deferred hydration may publish an estimated view followed by
a hydrated refinement. Dispatch p50/p95/p99/maximum were
324/420/509/1,204 us, with one metadata reader, one hydration reader, and zero
measured-interval opens. The 600-frame sparse
autonomous rerun used 35,737 us and 23,336,589 bytes versus 35,887 us and
22,936,808 bytes when hydrated. Sparse/hydrated ratios were 0.996 for frame total,
1.005 for allocation count, and 1.017 for bytes, with zero payload loads.

The final correctness and release gates are:

- 5,167 workspace tests passed with 3 skipped;
- 86.25 percent line coverage passed the 80 percent gate;
- strict full-workspace clippy, explicit `harness,transcript-bench` clippy, and
  formatting passed;
- Lua API stubs and reference documentation regenerated successfully;
- permission and transcript/tool storybooks passed and changed snapshots were
  manually inspected;
- the optimized `release-fast` smelt build passed;
- `git diff --check` passed and no generated `.snap.new` files remained.

### Autonomous streaming investigation reference

At a fixed 128 provider output chunks, complete active output size controlled the
frame tail and allocation. The visible tool body remained capped at 20 rows:

| Provider bash output | Frame p99 | Total allocated |
|---:|---:|---:|
| 8 KiB | 3.30 ms | 182 MB |
| 32 KiB | 6.61 ms | 463 MB |
| 128 KiB | 22.97 ms | 1.61 GB |
| 1 MiB | 47.40 ms | 3.28 GB |
| 4 MiB | 85.92 ms | 5.31 GB |
| 8 MiB | 153.37 ms | 7.85 GB |

A separate one-chunk test removed event-count amplification and reproduced a
multi-second frame from full active-tool-state preparation alone. A 64 MiB output
produced a 1.428-second frame. A 96 MiB output produced 2.088-second
`app:tick_compositor` and 2.087-second `compositor:project_transcript` p99 values,
with 981 MB allocated in the worst frame. In that frame path, Lua layout
compilation reached 1.451 seconds and 456 MB, complete-output display hashing ran
13 times and consumed 1.071 seconds in aggregate, row-index preparation reached
218 ms, and visible-range collection was 139 ms. The 20-row visual cap bounded
terminal rows but did not bound hashing, snapshots, layout compilation, or row
indexing.

Fixed 64 KiB provider output also scaled with event count: 32 chunks took 200 ms
and 196 MB, while 512 chunks took 3.81 seconds and 3.99 GB. The observed model is
`O(events * accumulated_output)` when every urgent delta promotes full active
state into a frame.

Incremental tool-call argument parsing has a separate quadratic path. Every
character in a streamed JSON string clones the growing string and reinserts it
into the preview map, and final replacement repeats parsing:

| Streamed `write_file` content | Frame p99 | Max lifecycle dispatch | Total allocated |
|---:|---:|---:|---:|
| 64 KiB | 33.22 ms | 32.10 ms | 4.58 GB |
| 128 KiB | 59.21 ms | 115.94 ms | 18.10 GB |
| 192 KiB | 90.31 ms | 253.00 ms | 40.56 GB |

Grouped tool state is rebuilt, serialized, and hashed as a whole when one child
changes. With 16 chunks of roughly 1 MiB per child, one child produced a 1.67 ms
frame p99 and 19.3 MB allocation; eight children produced 60.87 ms and 4.86 GB.
The eight-child sample spent 860 MB on group snapshots and about 1.00 GB on
display hashing.

Local exec output has a further clone-and-rewrite cost. Its append path allocated
0.83 MB for 64 chunks and 8 KiB, 12.78 MB for 256 chunks and 32 KiB, and 50.94 MB
for 512 chunks and 64 KiB. This is proportional to the sum of accumulated output
sizes, not only newly appended bytes.

### Sparse and resumed autonomous frames

Sparse tail planning can run on autonomous animation frames even when visible
projection is reused. Over 600 one-byte active-turn frames, a hydrated transcript
took 97.6 ms and allocated 47.5 MB. A sparse 5 MiB tail took 1.573 seconds and
allocated 1.220 GB, while visible projection ran only twice. In a 100-frame
attribution sample, sparse planning consumed 244.7 ms and 195.7 MB. Separate
loaded-prefix and scrollbar-total calculations each spent about 120 ms and
allocated about 97 MB.

Both calculations reconstruct partial persisted extent boundaries by hydrating
and deserializing object-backed records. With 600 records of 64 KiB each, 100
frames reached 7.82 ms p99 and allocated 1.81 GB. With 512 KiB records, one extent
query deserialized 23 records, about 12.1 MB of transcript text, in 11.7 ms and
allocated 47.9 MB. A temporary retained partial-extent cache reduced the ordinary
5 MiB, 100-frame sample from 277 ms to 28 ms and from 212 MB to 18 MB. Sparse
planning fell from 244.7 ms to 1.6 ms. The cache experiment was reverted after
measurement.

Committed-view observers are a distinct sparse cost outside the main transcript
projection child span. The built-in scroll-pill watcher calls
`view:previous_block({ role = "user" })` whenever the committed view changes.
The persisted fallback reads up to 256 complete records to find a role match,
even when the contiguous loaded window already contains a sufficient nearby
candidate. With 512 KiB records, one callback read 256 records in 141.7 ms and
allocated 533 MB. Committed-view dispatch reached 142.0 ms and 534 MB, producing
a 158.1 ms frame.

A temporary loaded-candidate shortcut removed that persisted scan on the same
fixture. Frame p99 fell from 158.1 ms to 17.4 ms, and total allocation fell from
659 MB to 125 MB. The remaining 17 ms was the separately attributed extent work.
The shortcut was reverted because the durable design should use bounded semantic
navigation metadata and prove range coverage, rather than rely on an unconditional
loaded-result preference.

The relevant asymptotic costs are:

- urgent provider output: `O(events * frame_cost)`;
- full active output preparation: `O(events * accumulated_output)`;
- streamed JSON draft preview: `O(argument_bytes^2)`;
- grouped tools: approximately `O(events * children * child_state_size)`;
- local exec replacement: `O(chunks * accumulated_output)`;
- sparse extent planning: `O(frames * boundary_records * boundary_payload_bytes)`;
- sparse semantic navigation: `O(view_changes * navigation_chunk_records * record_payload_bytes)`.

Reader-open spans are reported separately as `transcript:store:open_read_only`,
`session:live_store:open`, and `engine:model_history:open_store`. An active sparse
transcript normally retains one metadata reader plus one hydration-worker reader,
so repeated `store:lineage:open_read_only_located` counts must not be attributed to
each hydration range without matching caller counts.

### Architecture recommendations

The recommended path is an incremental retained transcript architecture, delivered
in independent slices rather than by tuning individual clone or hash loops:

1. Represent live tool, exec, text, and draft content as append-only chunks with a
   stable block identity and monotonic content revision. Maintain incremental
   hashes and parser state at append time. Layout input should contain metadata,
   revisions, and lazy source handles, not a JSON copy of complete output.
2. Coalesce all continuation deltas at the frame boundary, including provider tool
   output, local exec output, and draft updates. Keep first output, completion,
   errors, permission changes, and user interactions urgent. Backpressure should
   guarantee at most one ordinary continuation frame per display interval.
3. Make visual caps computational. Capped text needs retained row checkpoints and
   incremental ANSI/parser state so measuring or rendering the final 20 rows does
   not revisit the complete prefix. Adjacent row requests should be one range.
4. Store incremental JSON draft parser state without inserting a cloned growing
   string after every character. Materialize an immutable render snapshot only
   when a scheduled frame consumes a changed draft.
5. Cache grouped tools per child. A parent group key should contain child IDs,
   child revisions, grouping policy, and presentation state. Updating one child
   must not serialize or hash every sibling's complete output.
6. Retain sparse extent indexes by immutable transcript root, width, and active
   range. Prefix and total-row consumers should share one result. Persisted extent
   chunks should answer partial boundaries without deserializing record payloads.
7. Persist semantic navigation indexes by role and kind, or equivalent nearest
   neighbor links. Build committed transcript views with bounded previous/next
   targets so Lua observers cannot trigger synchronous storage scans.
8. Replace local exec clone-and-rewrite with the same append-only live block used
   for provider output. Keep terminal decoding and row checkpoints incremental.
9. Keep lineage readers persistent per immutable store address or route reads
   through a batched hydration service. Synchronous committed-view callbacks and
   frame preparation should perform no storage opens or unbounded reads.
10. Separate animation invalidation from transcript invalidation. A spinner frame
    should reuse transcript extents, navigation targets, layouts, and visible rows
    unless a corresponding revision changed.

The principal alternative is a narrower compatibility-preserving retrofit: add
continuation coalescing, incremental hashes, draft parser snapshots, partial
extent caches, and semantic navigation indexes behind current APIs. It is less
invasive, but the JSON/Lua full-snapshot boundary and cross-phase invalidation
remain easy places to reintroduce complete-state work.

A from-scratch design would use an immutable typed patch log as the source of
truth. A render store would apply patches to stable nodes backed by chunked text,
incremental parser state, per-child group caches, and width-specific row indexes.
A persisted extent tree would combine exact loaded heights with stored chunk
profiles, while persisted role/kind indexes would provide semantic neighbors.
Viewport queries would return only intersecting retained rows. Lua renderers would
receive metadata and bounded lazy source views. A frame scheduler would merge
semantic invalidations, compute one row diff, and let the compositor paint only
changed terminal cells. This provides the clearest ownership and asymptotic
bounds, but it requires replacing the current snapshot-oriented renderer contract.

### Correlation with recent changes

The problems accumulated across several features rather than one regression:

- `0a78ae88a` introduced streamed tool-call drafts on 2026-06-17, including the
  per-character growing-string snapshot behavior.
- `8ba5ff14f` added committed transcript views and the scroll-pill watcher on
  2026-07-19. `7e4b444cd` connected sparse navigation to both loaded and stored
  candidates on 2026-07-23, creating the redundant persisted comparison scan.
- `74fb50038` restored incremental engine output rendering on 2026-07-20.
  `ab7e7f44d` later coalesced reasoning and text continuations but left
  `ToolOutput` urgent, preserving frame-per-event amplification for tool output.
- `f777c1b83` added resumed transcript virtualization and persisted extent prefix
  use on 2026-07-28. Its uncached partial-boundary fallback is the repeated sparse
  extent cost measured here.
- `3203013ee` made tool rendering Lua-owned on 2026-07-31, moving complete tool
  snapshots across the Lua layout boundary. `49be13743` integrated that renderer
  with transcript groups, where one-child invalidation now promotes whole-group
  state.
- `ab7e7f44d` changed several conversation, scheduler, hash, layout, and sparse
  hot paths on 2026-08-21. It improved earlier cases but did not establish a
  bounded incremental contract, so the older costs still compose in current
  autonomous frames.

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
- real previous-user and bottom scroll-pill mouse clicks plus redraw;
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

## Scroll pills and tall expanded `write_file`

The navigation fixture contains 4,000 user/assistant turns plus a tall final
response, producing 28,117 rendered rows. The pill samples locate the named
`smelt.scroll_pills.top.win` and `smelt.scroll_pills.bottom.win` overlays, send
real left-button down/up events at their rendered rectangles, run the Lua press
callbacks, and redraw. This covers the same event path as an interactive click,
not a direct call to transcript internals.

Five uncontended release samples produced:

| Interaction | Mean | Stddev | p50 | p95 | p99 | Maximum |
|---|---:|---:|---:|---:|---:|---:|
| Previous-user pill | 0.549 ms | 0.005 ms | 0.552 ms | 0.552 ms | 0.552 ms | 0.552 ms |
| Bottom pill | 0.397 ms | 0.014 ms | 0.393 ms | 0.420 ms | 0.420 ms | 0.420 ms |

The previous-user path used to obtain an exact row by compiling and measuring
all loaded render nodes. It now prepares or reuses the height index, exactifies
only the target node, and carries that node's stable row anchor into projection.
The benchmark rejects full-session reads, full block-buffer renders, complete
height-index rebuilds, more than 128 exactified target/viewport nodes, more than
1,024 materialized viewport rows, or failure to reuse the existing index. A
400-node unit fixture more tightly verifies that resolving the target itself
compiles and measures exactly one node and materializes no row range.

The tall source-view fixture starts with a collapsed completed `write_file`,
presses real Enter to expand it, and scrolls 12 rendered wheel frames at the top,
middle, and 90 percent depth of 20,000 Rust source lines. Five release samples on
the same machine produced:

| Expanded `write_file` phase | Mean | p95 |
|---|---:|---:|
| Collapsed 12-frame scroll | 2.529 ms | 2.831 ms |
| Enter and expansion | 14.177 ms | 14.880 ms |
| Top 12-frame scroll | 8.865 ms | 8.925 ms |
| Middle 12-frame scroll | 11.464 ms | 11.524 ms |
| Deep 12-frame scroll | 11.417 ms | 11.590 ms |

The source was 1,448,889 bytes. Middle and deep latencies remain effectively
constant instead of increasing with source offset. The older
`tool_output_4mib` workload is retained as a broad raw-tool-output reference,
but it renders `bash` output and did not exercise the `write_file` syntax path.
Its three-run pre-fix release reference was 76.029 ms for 12 scrolls and
22.352 ms for visible-range materialization. Its five-run post-fix reference was
74.450 ms and 22.504 ms respectively, confirming that the targeted source-view
fix does not regress the unrelated workload.

The bottleneck was `print_diff_ir_with_width`: every deep range restarted
syntect at source line zero, syntax-highlighted and wrapped all preceding lines,
and discarded them until it reached the requested visual row. `DiffIr` now has
non-serialized runtime caches for at most two width-specific visual-row indexes
and syntax-state checkpoints every 128 source lines. A deep render finds the
source line containing the first requested row, restores the nearest compatible
syntax state, and processes only the bounded replay suffix and visible rows.
Changing syntax theme invalidates the syntax checkpoints, while serde round
trips intentionally discard all runtime cache state.

Every measured 12-frame scroll rejects full-session reads, full block-buffer
renders, complete height-index rebuilds, failure to reuse the existing index,
and more than 3,072 materialized rows. Expanded frames additionally require
fewer than 128 replayed prefix source lines and at most 256 highlighted source
lines per frame. Tests compare deep range text, wrapping, span styles, and
metadata with a full render across a multiline syntax scope, verify cache
serialization behavior, and enforce the two-layout and 513-checkpoint limits.

## Submit, persistence, and provider history

The save/request suite includes the actual prompt interaction instead of only
calling internal persistence methods:

```bash
# Row-count scaling with short messages.
cargo xtask bench-transcript-layout --runs 3 --skip-nav \
  --workloads tiny_blocks_1mib --save-request-history 10000

# Isolate Enter from unrelated layout and navigation benchmarks.
TMPDIR=~/tmp cargo xtask bench-transcript-layout --runs 20 \
  --save-request-only --save-request-operations submit_enter \
  --save-request-history 1024

# Generate mixed user, assistant, reasoning, note, tool, and large
# object-backed metadata rows.
TMPDIR=~/tmp cargo xtask bench-transcript-layout --runs 10 \
  --save-request-only --save-request-operations submit_enter \
  --save-request-history 1024 --save-request-heterogeneous

# Replay a copied lineage sessions root through normal resume and Enter paths.
TMPDIR=~/tmp cargo xtask bench-transcript-layout --runs 20 \
  --save-request-fixture ~/tmp/session-fixtures/sessions \
  --save-request-session-id <session-id> \
  --save-request-operations submit_enter

# Byte and memory scaling with 2,000 history items of at least 8 KiB each.
mkdir -p ~/tmp
TMPDIR=~/tmp cargo xtask bench-transcript-layout --runs 1 --skip-nav \
  --workloads tiny_blocks_1mib --save-request-history 2000 \
  --save-request-item-bytes 8192
```

The fixture path must be a complete copied sessions root containing lineage directories;
`--save-request-session-id` selects the branch to resume. The harness copies the
complete root into an isolated test home, resumes through the normal application
path, and mutates only that copy. Copy and resume setup are reported separately
and are outside the measured Enter interval. Never benchmark against a live
sessions root.

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

Summaries report mean, standard deviation, p50, p95, p99, and maximum latency.
Each sample separates total Enter time from persistence queue wait,
`submit_turn`, SQLite transaction commit, complete `agent:begin_turn`, and
project-context preparation. It also includes calling-thread allocations,
process-wide allocation and deallocation churn, retained allocator bytes before
and after the operation, full invariant-validation row count, derived
search-blob rows and bytes, and the number and bytes of user turns cloned during
submission. Perf rows expose storage row counts, object hydration, Lua callback
cost, and other nested phases. The process-level `BENCH_MEMORY_SUMMARY` reports
peak RSS for the complete isolated benchmark process.

Use a dedicated invocation for each size when comparing peak RSS. Peak RSS is a
process high-water mark and cannot be attributed to a later phase after a larger
fixture has already run. Put `TMPDIR` under `~/tmp` for 50 MiB and 500 MiB runs so
large temporary databases and spill files do not consume the limited `/tmp`
filesystem.

### Production-session Enter investigation

Synthetic short rows did not reproduce the reported multi-second send stall. The
investigation therefore inventoried production sessions read-only, copied selected
databases with SQLite's online backup API, and pressed Enter through the normal
resume, Lua callback, canonical persistence, durable receipt, and `StartTurn`
path. SQLite backup and resume time are outside the measured interval.

The first reproduced stall was not the durable commit. A 1,934-row, object-heavy
session spent about 1.76 seconds in a title-plugin Lua callback. The callback read
a bounded message list and then requested history only to count it. The resumed
`LiveSession` tail implementation hydrated and cloned up to 200 object-heavy
rows, repeatedly serialized the shrinking vector, removed rows from the front,
and repeated that work for both reads. The final implementation gives count-only
callers `smelt.session.history_len()` and reads tails newest-first under item and
hydrated-byte budgets. Object reference sizes are checked before hydration, each
accepted row is serialized once, and the in-memory and SQLite-backed paths have
the same bounds.

A second fixture exposed a deeper canonical-history problem. Clearing a named
context item at history index 6,526 physically removed that row and rewrote the
following 1,017 rows. Named context updates and clears are now append-only
semantic events. Updates state that they replace earlier same-name context;
clears append a model-visible tombstone. Identical current values remain no-ops.
This preserves canonical and provider-cache prefixes and makes Enter proportional
to the new event suffix instead of the distance to an old context row.

| Isolated release workload | Runs | Enter mean | Enter p95 | Enter max | First redraw mean | Peak RSS |
|---|---:|---:|---:|---:|---:|---:|
| 1,024 generated short rows | 30 | 14.346 ms | 14.899 ms | 15.270 ms | not sampled | 34,192 KiB |
| 1,934-row object-heavy copy | 20 | 30.216 ms | 30.966 ms | 31.188 ms | not sampled | 46,264 KiB |
| 758-row heterogeneous copy | 3 | 18.982 ms | 19.499 ms | 19.499 ms | 5.248 ms | 43,936 KiB |
| 7,543-row deep-context copy | 3 | 44.702 ms | 45.729 ms | 45.729 ms | 7.558 ms | 47,748 KiB |
| 19-row copy in a 2.1 GB database | 1 | 13.445 ms | 13.445 ms | 13.445 ms | 1.164 ms | 35,148 KiB |

On the primary object-heavy copy, Enter improved from 1,834.364 to 30.216 ms
mean and from 1,850.012 to 31.188 ms maximum. Peak RSS fell from 78,204 to
46,264 KiB. Allocation churn fell from at least 5.3 GB in the Lua callback alone
to about 10 MB process-wide. The 7,543-row context case improved from 2,629.844
to 44.702 ms mean, from 518,632 to 47,748 KiB peak RSS, and from about 1.3 GB to
7.9 MB of process-wide allocation churn. Its persistence work changed from deleting and
reinserting 1,017 rows to appending two rows.

The exact append decision is shared by materialized and resumed sessions. Resumed
planning uses scalar SQLite projections for visibility, note kind, context name,
and effective mode rather than hydrating payload rows. Canonical controls are
also checked between best-effort request-audit transactions, so queued audits do
not delay a new turn.

SQLite `synchronous = FULL`, one canonical transaction per Enter, and provider
dispatch only after the durable receipt remain unchanged. Representative final
commits took about 5 to 10 ms. Lowering durability or dispatching optimistically
would add correctness risk without addressing either reproduced stall.

## Complete-process startup and shutdown

The lifecycle benchmark launches the release-fast `smelt` executable in a real
PTY, not an application constructor or store microbenchmark. Each run copies a
fixture with SQLite's online backup API into fresh isolated HOME and XDG roots,
performs normal `--resume`, waits until unique tail content is visibly rendered,
sends Ctrl-C, and waits for clean process teardown. It measures launch to first
frame separately from launch to loaded session, because an early frame can be
responsive before the requested transcript is usable. It also reports
Ctrl-C-to-exit latency and samples process RSS on Linux.

The feature-gated integration harness can run against any safe copied lineage
sessions root. Every per-run copy is removed before the next run, so large fixtures
do not consume `runs` times their size:

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

Use a copied sessions root, never live state. The ready marker should be text in
the initially visible resumed tail. `SMELT_LIFECYCLE_BENCH_FIRST_FRAME_TEXT`
can override the default `local/test-model` marker, and
`SMELT_LIFECYCLE_BENCH_TIMEOUT_SECS` can raise the per-phase timeout. Timing begins
inside the test after Cargo has built the executable, so compiler work is excluded.

Sparse lineage resume is bounded by the loaded tail and viewport state, not
complete history length or database bytes. Keep fixture-copy time separate from
launch, visible-ready, and graceful-exit measurements so setup cost cannot hide an
interaction-path regression.

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

Benchmark setup explicitly requests the derived search projection and waits for it
to become current before any search timing begins. Format 2 stores immutable,
content-addressed search segments plus an ordered manifest for each canonical
transcript root. Forks and rewinds that share a root reuse the same projection.
Candidate lookup validates the current root in constant time, reads its compact
segment manifest, and loads canonical source leaves only for segments whose
candidate documents need exact literal verification. A missing, stale, corrupt,
or incompatible projection falls back to canonical direct search.

Search queries of at least three characters use SQLite FTS5 trigram candidates.
One- and two-character queries use compact per-segment postings. FTS rows are read
through bounded, keyset-paginated pages, and exact canonical verification batches
adjacent documents without allowing false positives to truncate a result page.
Rare, common, and absent queries are all benchmarked because latency scales with
posting and returned-candidate cardinality, not only total transcript bytes. The
benchmark rejects a measured search unless both the persisted candidate index and
the current root manifest were used.

The true-resume sample reports allocator churn and retained bytes around only the
sparse tail load and first render. Whole-process peak RSS also includes fixture
construction and is therefore only an upper bound for resumed-session memory.

## Active transcript retained memory

The active-memory suite builds an active canonical session in 256-block save
batches, applies each persistence receipt, drains bounded idle compaction, then
requests and settles the derived search projection outside every measured
interval. It measures first render, 20 Ctrl-D scrolls, indexed search, `n`
navigation, and one explicit 1,200-block hydration request. The request is larger
than the hydrated-content budget by design: worker completion must succeed while
bounded LRU retention may evict older members of the same batch. Reusing the
newest requested block that remains materialized must perform no additional
SQLite read. The benchmark fails if search does not use the candidate index and
current root manifest, committed full content remains live, any cache exceeds its
measured budget allowance, or a 50 MiB or larger workload does not exercise
hydrated-block eviction.

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
The measurements below used the `release` profile and these commands after
prebuilding:

```bash
TMPDIR=~/tmp cargo test --release -p smelt-tui \
  --features transcript-bench transcript_active_memory_benchmark_suite \
  --no-run

test_bin="$(find target/release/deps -maxdepth 1 -type f \
  -name 'tui-*' -perm -u+x -printf '%T@ %p\n' \
  | sort -nr | head -1 | cut -d' ' -f2-)"

/usr/bin/time -v env TMPDIR="$HOME/tmp" \
  SMELT_TRANSCRIPT_ACTIVE_MEMORY_BYTES=52428800 \
  "$test_bin" --exact \
  transcript_benchmarks::transcript_active_memory_benchmark_suite \
  --nocapture

/usr/bin/time -v env TMPDIR="$HOME/tmp" \
  SMELT_TRANSCRIPT_ACTIVE_MEMORY_BYTES=524288000 \
  "$test_bin" --exact \
  transcript_benchmarks::transcript_active_memory_benchmark_suite \
  --nocapture
```

The centralized defaults were 32 MiB for hydrated block content, 16 MiB for
loaded record windows, and 16 MiB for rendered payloads. Live block
metadata is owned by the boxed live entry; stored and hydrated entries derive
canonical hash and origin from `StoredBlockRef`, with `Done` status implicit.
Both workloads ended with zero live block, tool-state, and block-metadata
bytes, zero oversize debt, and zero SQLite rereads when immediately reusing the
newest hydration-churn block.

| Category | Active 50 MiB | Active 500 MiB |
|---|---:|---:|
| Generated bytes / blocks | 52,451,885 / 1,439 | 524,314,464 / 14,384 |
| Live full-content bytes | 0 | 0 |
| Hydrated block bytes | 33,533,562 | 33,533,570 |
| Stored / hydrated blocks | 561 / 878 | 13,506 / 878 |
| Compact record bytes | 1,174,224 | 11,737,344 |
| Record-window bytes | 0 | 0 |
| Tool-state index bytes | 0 | 0 |
| Block metadata bytes | 0 | 0 |
| Rendered payload bytes | 249,407 | 1,181,449 |
| Hydrated / rendered pinned bytes | 76,392 / 192,713 | 76,394 / 1,124,755 |
| Hydration reads / ranges | 1,201 / 8 | 1,203 / 7 |
| Hydration bytes / duration | 45,869,890 / 160.418 ms | 45,946,279 / 174.985 ms |
| Evicted entries / bytes | 323 / 12,336,328 | 325 / 12,412,709 |
| Dematerialized entries / bytes | 1,439 / 54,960,062 | 14,384 / 549,385,776 |
| Allocator retained delta | 41,368,033 | 55,967,090 |
| In-process RSS / peak RSS | 211,251,200 / 211,251,200 | 275,292,160 / 275,292,160 |

| Operation | Active 50 MiB | Active 500 MiB |
|---|---:|---:|
| Persist, compact, and project fixture | 5,380.212 ms | 76,378.301 ms |
| First render | 3.551 ms | 10.195 ms |
| 20 Ctrl-D scrolls | 8.439 ms | 9.274 ms |
| Indexed search and reveal | 15.221 ms | 50.174 ms |
| `n` navigation and reveal | 8.508 ms | 10.498 ms |
| One 1,200-block hydration request | 179.813 ms | 195.118 ms |
| Working-set SQLite rereads | 0 | 0 |

The 500 MiB workload retained only eight additional hydrated-content bytes. Its
additional retained memory came from compact per-block records, mappings, the
format-2 derived search projection, and allocator overhead. It did not retain an
additional 450 MiB copy of committed content. The single 1,200-ID request is
deliberately larger than cache capacity and settled in only seven or eight
coalesced reader ranges, rather than 1,200 worker round trips. Successful worker
completion does not require every requested block to remain simultaneously
materialized after bounded-cache eviction. Normal rendering and navigation use
smaller coalesced viewport/range requests. Compact records and indexes remain
proportional to block count, while fully active uncommitted model history remains
proportional to its content until a durable receipt makes it eligible for idle
dematerialization.

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
record payload bytes as both
hydrated and pinned, with zero evicted bytes, matching the permanent `OnceLock`
cache baseline. The full command output was retained during implementation as
`~/tmp/smelt-phase0-release-smoke.log`.

At 50K rows, Enter is 54% faster and first redraw is 87% faster. At
2K x 8 KiB, Enter is 54% faster and its allocation churn falls from 80.77 MiB
to 12.96 MiB. Engine materialization allocation churn falls from 103.33 MiB to
72.22 MiB at 50K rows and from 97.94 MiB to 49.74 MiB for the byte-heavy case.
The small row-heavy peak-RSS increase is transient fixture/index construction;
steady allocator state is unchanged and the byte-heavy peak is within 1%.

Submission now performs no full history invariant scan, no derived filesystem
writes, and no user-turn cloning in the Enter barrier. The last-user lookup
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
For 240 resumed wheel frames, record aggregate time fell from 111.667 ms to
57.191 ms and wall time fell from 1004.463 ms to 889.067 ms.

### Canonical session architecture final validation

The final Phase 7 validation ran on 2026-07-21 with `TMPDIR=~/tmp`. The layout,
Enter, search, sparse-resume, resumed-wheel, and catalog suites used the release
profile. Active-memory measurements also prebuilt the release executable so
rustc memory was excluded. Each large workload ran in an isolated test process. The `BENCH_MEMORY_SUMMARY` high-water mark is reported for xtask
suites; direct-test `/usr/bin/time -v` RSS is reported for catalog and active
memory.

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
history suffix rows, one record suffix row, zero invariant history rows,
zero search-blob rows, one last-user block scanned, and at most two record
rank entries scanned. Provider dispatch followed the durable receipt in every
sample. Catalog projection was queued after durability and was not awaited by
Enter. Benchmark setup waited for the prior fixture revision's catalog update
before clearing metrics, so prior asynchronous work did not contaminate the
measured interval.

Search used the commands in "Large search and resume" above, with an explicit
`--search-bytes 524288000 --no-warmup` for 500 MiB. Both sizes ran five times to
quantify fixed-cost variance. Indexed candidate pages remained bounded at 512
blocks. Cached `n` navigation did not repeat full candidate scans, and record
hydration used known canonical extents rather than recounting the whole record
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
summary recorded zero foreground record-window loads during the measured
wheel loop and only two row-index rebuilds.

Final active-memory measurements used the direct commands shown above. Raw
content remained bounded by the 32 MiB hydration budget, with zero live committed
blocks, zero oversize debt, and zero rereads of the newest working-set block.

| Category | Final active 50 MiB | Final active 500 MiB |
|---|---:|---:|
| Generated bytes / blocks | 52,451,885 / 1,439 | 524,314,464 / 14,384 |
| Stored / hydrated blocks | 524 / 915 | 13,469 / 915 |
| Hydrated block bytes | 33,536,786 | 33,536,788 |
| Compact record bytes | 1,070,616 | 10,701,696 |
| Block metadata bytes | 0 | 0 |
| Rendered payload bytes | 256,801 | 1,188,844 |
| Allocator retained delta | 38,569,862 | 51,585,638 |
| In-process peak RSS | 117,866,496 B | 130,662,400 B |
| External peak RSS | 114,168 KiB | 126,040 KiB |

| Operation | Final active 50 MiB | Final active 500 MiB |
|---|---:|---:|
| Persist and compact fixture | 2,268.472 ms | 27,389.458 ms |
| First render | 2.040 ms | 5.298 ms |
| 20 Ctrl-D scrolls | 5.918 ms | 6.133 ms |
| Indexed search and reveal | 3.694 ms | 6.201 ms |
| `n` navigation and reveal | 2.040 ms | 4.523 ms |
| Hydrate up to 1,200 blocks | 110.791 ms | 113.179 ms |
| Working-set SQLite rereads | 0 | 0 |

The cached hydrated-membership and retained-byte invariants removed transcript-
length scans from budget enforcement. Compared with the Phase 5 measurements,
the 500 MiB first render improved from 9.251 to 5.298 ms, 20 scrolls from 67.555
to 6.133 ms, search from 12.550 to 6.201 ms, `n` navigation from 8.000 to 4.523
ms, and adversarial hydration from 2,188.738 to 113.179 ms. The 500 MiB
in-process peak RSS was only 12.2 MiB higher than the 50 MiB process, not another
450 MiB copy of committed content.

### Post-Phase 7 projected-search and hydration hardening

The dedicated search and active-memory fixtures now request and settle the same
production `LineageSearchProjector` used by interactive sessions before clearing
perf counters or starting a timed operation. Every indexed sample asserts both
`search:transcript:sqlite_available = 1` and
`store:lineage:search_manifest_available = 1`. This caught an earlier benchmark
configuration that measured direct canonical fallback while labeling it indexed.

Format 2 publishes a root manifest atomically only after all immutable source
segments are complete. Candidate lookup obtains the canonical root identity in
constant time, reconstructs leaves lazily only for selected segments, verifies
FTS candidates against canonical records in bounded batches, and paginates false
positives without sacrificing literal correctness. The latest single-run release
samples were:

| Search operation | 50 MiB before root manifests | 50 MiB format 2 | 500 MiB before root manifests | 500 MiB format 2 |
|---|---:|---:|---:|---:|
| Rare FTS submit | 71.189 ms | 38.591 ms | 404.847 ms | 199.065 ms |
| Common one-character submit | 54.780 ms | 33.342 ms | 241.055 ms | 60.618 ms |
| Absent one-character submit | 37.634 ms | 1.250 ms | 404.268 ms | 45.224 ms |
| Common FTS submit | 50.217 ms | 42.373 ms | 243.482 ms | 75.130 ms |
| 100 cached `n` jumps | 140.776 ms | 152.582 ms | 174.196 ms | 193.757 ms |
| Sparse rare submit | 258.599 ms | 186.902 ms | 1,885.011 ms | 1,333.438 ms |
| Append then repeat search | 43.543 ms | 3.748 ms | 409.717 ms | 15.312 ms |

The rare indexed page returned 55 candidates at 50 MiB and the full bounded 512
at 500 MiB, so its remaining growth is output-sensitive exact verification rather
than a canonical transcript walk. Absent and append searches load no candidate
blocks. Every measured query used the current root manifest; indexed candidate
pages stayed at or below 512 blocks, exact app-level refinement scanned at most
six entries, and the appended dirty suffix scanned one candidate.

The oversized resumed fixture contains 600 records, but only records 0, 300, and
599 carry the configured large payload. This exercises top, middle, and tail
extent boundaries without retaining 600 artificial large records. All three
8 MiB boundary positions passed the scheduled release gate. Representative top
and middle samples processed a 25,204,032-byte resumed fixture with two persistent
readers, one hydration-reader open, zero metadata-reader opens, and 0.807 ms and
1.121 ms frame p99 respectively.

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

The Phase 7 raw outputs were retained at:

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

### Complexity and remaining limits

- Enter persistence is proportional to the changed history and record
  suffix, transactional search-index work, and SQLite commit. Catalog projection
  is queued only after the canonical receipt and is not awaited.
- First redraw after an append is proportional to the appended render-plan
  suffix and visible projection work. A bounded mutation log falls back to a
  full rebuild after it can no longer prove incremental safety.
- Provider request construction is necessarily linear in active model-history
  bytes, but the engine now performs one message conversion, no token-estimate
  byte-buffer allocation, and no unchanged-prefix snapshot clone.
- FTS search scales with posting and candidate cardinality. Covered
  one-character search scans compact per-block masks rather than transcript
  text. Two-character and out-of-range one-character searches still scan text.
- Sparse resume memory is proportional to the record window and rendered
  tail, not total transcript size. Fully active, uncheckpointed model history
  remains linear in active history size and should be checkpointed for very
  large sessions.
- Session listing reads the rebuildable catalog. Search and resume read canonical
  SQLite directly, so no transcript-sized compatibility export exists.
