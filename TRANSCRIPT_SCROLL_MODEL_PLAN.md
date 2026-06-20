# Transcript Scroll Model and Sparse Row Extent Plan

## Purpose

This plan refines the transcript virtualization work around one specific question: how scrolling should map between user-visible content, virtual document rows, sparse descriptor windows, and scrollbar position without loading the full transcript.

The current branch has better boundedness and fewer obvious sparse-scroll regressions than the starting point, but user testing still reports inconsistent velocity, lag, and perceived snap-back. The architecture should not keep estimation or numeric row identity as hidden dependencies in places where semantic content identity is required. The final model should make user intent, exact content anchors, bounded estimates, and resolved paint rows separate concepts.

This plan does not implement code. It records the target model, the current code seams, benchmark baseline, and the refactor phases needed to finish the transcript scrolling architecture without leaving debt.

## Post-Phase 6 reassessment

The first six phases improved scalability and removed several obvious sparse-scroll regressions, but they did not finish the migration to the correct scroll model. User testing still reports inconsistent velocity, lag, and occasional perceived snap-back while wheel scrolling or drag-selection autoscrolling. That means this document must no longer treat the Phase 6 result as final.

The core mistake was leaving numeric `scroll_top` as an authoritative transcript scroll state in too many paths. Sparse virtualization makes numeric rows unstable because unloaded prefix estimates, exact height observations, descriptor-window hydration, and render-plan refinement can all change the mapping between row number and visible content. The model must be corrected so user interactions are represented as semantic scroll intents over content, and `Window::scroll_top` becomes a resolved paint output for transcript windows.

The next work must prioritize architecture and observability over local bug patches:

1. Define the transcript scroll contract explicitly.
2. Add trace instrumentation that can explain real bad sessions.
3. Add replay and velocity tests that fail on the observed user experience, not only on internal monotonicity.
4. Move transcript scrolling to document-owned viewport state and explicit intents.
5. Delete or isolate row-number authority from transcript interaction paths.

## Principles carried forward

This plan inherits the principles from `VIRTUAL_TRANSCRIPT_DOCUMENT_VIEW_PLAN.md` directly. The scroll-model work is a refinement of that plan, not a separate architecture.

### Base principles from `VIRTUAL_TRANSCRIPT_DOCUMENT_VIEW_PLAN.md`

1. **Greenfield posture**
   - We are not constrained by current internal transcript layouts, row-index caches, viewer mode flags, or host override APIs.
   - We may replace display caches, row caches, transcript projection APIs, and window interaction APIs without compatibility shims.
   - The compatibility promise remains loading old sessions through storage-boundary importers for the compatibility window.

2. **Right abstractions over small patches**
   - Do not add more `TRANSCRIPT_WIN` branches to paper over a missing document boundary.
   - Do not make `Window` the semantic owner of row-backed text.
   - Do not make `Buffer` both source truth and projected render cache.
   - Promote the existing `DisplayDocument` seam instead of inventing a sibling transcript trait.

3. **One owner per concept**
   - Durable transcript records live in `smelt-store` SQLite tables.
   - Logical transcript state lives in `TranscriptDocument`.
   - Viewer cursor, selection, anchor, and follow-tail state live in `DocumentViewState`.
   - Vim parsing emits semantic `DocumentCommand`s.
   - Rendering materializes rows into a bounded `RenderCache`.
   - `Window` paints cached rows and owns terminal geometry, not document semantics.

4. **Exact interaction correctness**
   - Visible rows are exact.
   - Cursor position, hit testing, visual selection, copy/yank, search match location, fold targeting, link/action hit testing, `gg`, `G`, page movement, and pin-to-bottom behavior are exact.
   - The only allowed approximation is global scrollbar extent and coarse scrollbar click/drag mapping before local refinement.
   - Approximate rows must never provide text, copy data, action targets, or semantic cursor positions.

5. **No duplicate half-abstractions**
   - Every introduced type must have an explicit replacement and deletion target.
   - Transitional adapters are allowed only at boundaries and must have removal criteria.
   - Do not leave `DisplayDocument`, `UiHost::display_rows_for_range`, materialized-row `Window` mode, and transcript-specific app snapping all claiming to own row-document behavior.

6. **Do worthwhile large work now**
   - If a rename, split, or deletion makes the final architecture easier to reason about, include it.
   - Avoid intermediary scaffolding whose only purpose is to make the old architecture survive longer.
   - Delete obsolete eager transcript rebuild and host-special-case paths once the replacement covers load, live append, render, search, copy, and tests.

7. **Plan is allowed to evolve**
   - Update this plan when implementation uncovers better facts.
   - Changes should be explicit and justified.

8. **Measure first, then delete obsolete optimizations**
   - Micro-optimizations added to make the old architecture tolerable are not sacred.
   - If a better owner or database query removes the need for a cache, fallback, or reuse branch, delete the old optimization instead of layering the new design on top.
   - Keep performance instrumentation, but remove duplicated runtime paths once measurements show the replacement covers the operation.
   - Prefer simple typed transactions and indexed SQL over in-memory mirrors of database state.

9. **No intra-plan compatibility debt**
   - This rewrite lands as one unreleased change. No agent is running against intermediate Phase 1, Phase 2, Phase 3, or Phase 4 database shapes.
   - Do not add legacy readers, schema migrations, format-version bumps, cache-version bumps, or compatibility shims for formats introduced earlier in this plan.
   - When schema, cache, index, or wire shapes introduced by this plan need to change, rewrite the original definition in place and keep its version at the plan baseline.
   - The only compatibility code allowed here is for data formats that existed before this plan started, and it must stay isolated at the storage boundary.

### Scroll-model refinements

1. **No full transcript load on normal resume, render, scroll, search, copy, resize, append, or save.**
   Explicit import, export, repair, diagnostics, and focused tests may stream or materialize the whole session, but hot paths must not.

2. **Use exact rows whenever exact rows are required or already cheap.**
   The viewport, overscan, hit testing, copy/yank, selection, search targets, fold/action targets, and active cursor positions must use exact rendered rows, not estimates.

3. **Use estimates only for unloaded global extent and coarse seeking.**
   Approximation may inform scrollbar extent, scrollbar click/drag landing, and the initial descriptor range to load for a far jump. It must not provide text, action targets, cursor positions, copied data, or selected content.

4. **Stable estimates are mandatory.**
   Estimates must not depend on the current sparse window average. Loading a new window must not remap already visible content.

5. **Content identity beats numeric row identity.**
   The primary scroll state should be a semantic anchor, such as descriptor ordinal or block id plus intra-block row offset. Numeric `scroll_top` is a projection of that anchor into the current row-extent model, not the source of truth during sparse refinement.

6. **The scroll extent model belongs to `TranscriptDocument`.**
   `TranscriptDocument` owns sparse descriptor loading, row extents, exactification, render-cache policy, search/copy/fold/action materialization, and mapping between content anchors and rows. `Window` owns terminal geometry and paints resolved rows. `smelt-store` owns durable descriptor metadata and aggregate extent queries.

7. **Measure before changing performance-sensitive behavior.**
   Benchmarks must run with temporary files under `/home/dev/tmp`, not `/tmp`, to avoid filling tmpfs or swap.

## Current branch facts

### Relevant current code

- `TranscriptDocument` owns the current sparse transcript state, active descriptor range, session store identity, render cache, and descriptor estimate cache in `crates/tui/src/app/transcript.rs:382`.
- Current sparse row estimates are keyed by width and descriptor range with `DescriptorRowsEstimateKey` in `crates/tui/src/app/transcript.rs:417`.
- Current estimate lookup opens the SQLite store and calls the store aggregate from `estimated_descriptor_rows_for_range` in `crates/tui/src/app/transcript.rs:618`.
- Prefix/suffix row offsets are currently computed in `sparse_prefix_row_offset`, `sparse_suffix_rows`, and `virtual_total_rows` in `crates/tui/src/app/transcript.rs:647` and `crates/tui/src/app/transcript.rs:674`.
- Loaded-window exact/estimated row indexing lives in `TranscriptHeightIndex`, `TranscriptHeightNode`, and related row-index reuse logic in `crates/tui/src/content/transcript_buf.rs:123`.
- `Window` still stores and resolves numeric `scroll_top` and tail-follow state in `crates/edit/src/window.rs:790`, `crates/edit/src/window.rs:1975`, and `crates/edit/src/window.rs:2678`.
- Store aggregate estimate is currently `transcript_descriptor_estimated_rows` in `crates/store/src/history.rs:482` and is exposed by `SessionDb` in `crates/store/src/db.rs:431`.
- App-level regressions now cover heterogeneous sparse wheel-up scrolling and streaming snap-back in `crates/tui/src/app/harness_tests/misc.rs:818` and `crates/tui/src/app/harness_tests/misc.rs:865`.

### What is already better

The recent fix moved sparse prefix/suffix estimates from a loaded-window average to persisted descriptor metadata. That removes the largest source of jitter:

```text
old: missing rows = missing descriptor count * average rows in current loaded window
new: missing rows = SQLite aggregate over persisted descriptor metadata
```

The result removed one major instability source because loading a new descriptor window no longer changes the estimate for unrelated unloaded ranges based on local content mix. It did not make scrolling correct by construction because user gestures can still be reduced to numeric rows before transcript projection resolves them against semantic content.

### What is still architecturally wrong

The current fix is still a transitional model:

1. Estimate lookup is wired directly into `TranscriptDocument` as ad hoc range math instead of through a row-extent abstraction.
2. `virtual_total_rows` can require large unloaded-prefix/suffix estimates even for operations that only need exact tail or exact visible rows.
3. The store aggregate scans descriptor rows for large ranges. In the latest 500 MiB resume benchmark, one estimate over 127,920 unloaded descriptors took about 23 ms and contributed to a 45 ms tail load.
4. Numeric `scroll_top` still carries too much semantic meaning. It should be derived from a content anchor and current extent index, not treated as the durable scroll identity through sparse refinement.
5. There are two related but separate models: loaded render-plan row indexing in `transcript_buf.rs` and sparse unloaded-gap estimation in `app/transcript.rs`. They should be coordinated by one owner.

### Post-Phase 6 observed failure mode

The remaining lag and inconsistent scroll speed are consistent with a deeper state-model problem, not a single missing anchor case:

1. Wheel, scrollbar, and drag-autoscroll input are converted too early into absolute numeric row requests.
2. Projection later resolves those rows through a sparse extent model whose estimates may have changed.
3. Exact height observations and descriptor-window changes can update the row mapping during the same user gesture.
4. The render loop also preserves cursor screen rows, resolves tail-follow, coalesces wheel input, and applies materialized rows, so several subsystems can reinterpret the same gesture.
5. Current tests mostly assert monotonic row numbers or bounded work. They do not assert constant user-visible velocity, per-frame latency, or that the top visible content anchor advances by the intended amount on every input tick.

The corrected model must make this impossible by design: a user scroll gesture is an intent relative to current visible content. Numeric rows are only the resolved output after transcript projection has consumed that intent.

## Latest benchmark baseline

Benchmarks were rerun with temporary files under the home temp directory:

```bash
cd /home/dev/dev/smelt/.worktrees/transcript-virtualization
TMPDIR=/home/dev/tmp cargo xtask bench-transcript-layout \
  --runs 1 \
  --workloads mixed_10mib \
  --search \
  --search-bytes 524288000 \
  --resume \
  --resume-bytes 524288000 \
  --no-warmup
```

Output was also written to:

```text
/home/dev/tmp/smelt-transcript-scroll-model-bench.txt
```

### 10 MiB mixed layout workload

```text
TRANSCRIPT_LAYOUT_BENCH_SAMPLE workload=mixed_10mib run=1 input_bytes=10497943 generated_bytes=10499021 blocks=3404 rows=141762 first_ms=15.604 resize_ms=3.300 theme_ms=3.106 scroll12_ms=20.858 visible_ms=3.355 copy_ms=14.538 append_ms=7.468 no_cache_ms=15.517 allocs=35188 bytes_allocated=63187803
```

Important counters:

```text
first.full_row_builds=0
resize.full_row_builds=0
theme.full_row_builds=0
scroll12.full_row_builds=0
visible.full_row_builds=0
copy.full_row_builds=0
append.full_row_builds=0
```

Interpretation: viewport projection and local scrolling are already bounded by visible or overscan work in this workload. The plan should preserve that.

### 500 MiB search/view workload

```text
TRANSCRIPT_SEARCH_BENCH_SAMPLE run=1 bytes=524290206 rows=6413965 width_resize_ms=249.746 height_resize_ms=212.955 theme_color_ms=170.279 copy_mid_ms=2.453 nav_ctrl_d20_ms=117.262 nav_ctrl_u20_ms=256.623 nav_gg_ms=12.315 nav_G_ms=147.014 rare_ms=110.424 common_submit_ms=5.767 next100_ms=266.646 after_append_ms=15.213
```

Important bounded-work counters from the run:

```text
after_append search: dirty_candidate_blocks=1, dirty_candidates_scanned=1, render_plan:reused=1
after_append collect_nodes_range: rows=42 total, blocks=4 total
```

Interpretation: after-append search remains bounded. The resize/theme/navigation numbers are higher than the previous documented 500 MiB pass and should be treated as the current baseline for this worktree before the scroll-model cleanup. The plan must not make them worse, and should investigate whether estimate/anchor coupling is causing unnecessary row-index work during those app-level operations.

### 500 MiB descriptor-backed resume workload

```text
TRANSCRIPT_TRUE_RESUME_SAMPLE mode=descriptor_backed target_bytes=524288000 generated_bytes=524288000 descriptors=128000 rows=5377357 setup_ms=16461.129 tail_load_ms=45.421 tail_render_ms=24.524
```

Relevant perf counters:

```text
store:transcript:descriptor_estimated_rows count=1 last_us=23209
store:transcript:descriptor_estimated_rows_requested last=127920
store:transcript:descriptor_count last_us=18716
store:transcript:descriptor_slice_requested last=80
store:transcript:descriptors_loaded last=80
transcript:descriptor_window:loaded last=80
```

Interpretation: descriptor loading remains bounded to 80 descriptors, but computing total/prefix estimated rows currently scans the unloaded descriptor range. That is acceptable as a correctness bridge, but it should not remain on the first tail-render hot path.

## Correct model

The transcript should have four row-knowledge levels. Code should choose the most exact level that is available and required for the operation.

Before the level model matters, the scroll contract must be explicit:

1. **User scroll is intent, not a row assignment.**
   Wheel, keyboard page movement, drag autoscroll, scrollbar click/drag, search jump, resize reflow, and tail-follow are distinct intents. They must not all collapse into `ExactRow(scroll_top)` before `TranscriptDocument` sees them.

2. **Transcript viewport state is semantic.**
   The durable viewport state is content identity plus an offset, such as descriptor/block anchor and intra-anchor row offset. `Window::scroll_top` is a resolved paint coordinate for the current projection.

3. **Estimate refinement cannot move visible content.**
   Exact height observations and unloaded estimate improvements may move the scrollbar thumb or total extent. They must not reinterpret an in-flight wheel or drag gesture as a different content position.

4. **Velocity must be stable in content space.**
   A repeated wheel tick or autoscroll tick should advance by the same content-row amount unless it reaches a real boundary or the next content is not yet loaded. It must not become faster or slower because an estimate changed.

5. **Sparse placeholders are not a normal local-scroll result.**
   Placeholder rows may appear only for intentional far seeks into unloaded sparse gaps. Nearby wheel scrolling and drag autoscroll must load adjacent descriptors or stay anchored to the last exact content boundary.

6. **Only transcript projection resolves transcript scroll.**
   App, UI, and `Window` code may collect geometry and user events, but they should not apply transcript-specific sparse row math. They pass scroll intents to `TranscriptDocument`, which returns a resolved viewport.

### Transcript scroll intents

Introduce an explicit transcript intent type. The exact shape can evolve, but the model should distinguish at least:

```rust
enum TranscriptScrollIntent {
    Tail,
    PreserveViewport,
    UserDelta { rows: isize },
    PageDelta { pages: isize },
    ExactContentAnchor(TranscriptScrollAnchor),
    SearchJump(TranscriptScrollAnchor),
    ResizeReflow { previous_width: u16 },
    ScrollbarFraction { numerator: u64, denominator: u64 },
    ApproximateRowSeek(RowIndex),
}
```

Rules:

- Wheel and drag autoscroll use `UserDelta`.
- Scrollbar drag/click uses `ScrollbarFraction` or `ApproximateRowSeek` and is allowed to use estimates for the initial landing.
- Search and reveal use semantic anchors.
- Resize uses reflow preservation, not an exact numeric row.
- Tail-follow is its own state and does not require computing the full exact total before rendering.

### Transcript viewport state

`TranscriptDocument` should own the durable transcript viewport state:

```rust
struct TranscriptViewportState {
    top_anchor: Option<TranscriptScrollAnchor>,
    top_offset_rows: isize,
    mode: TranscriptViewportMode,
    pending_intent: Option<TranscriptScrollIntent>,
}

enum TranscriptViewportMode {
    Tail,
    Anchored,
    FarSeek,
}
```

Rules:

- `Window::scroll_top` for the transcript is updated from the projection result.
- `Window` should not be the transcript source of truth for follow-tail, sparse scroll identity, or semantic viewport anchoring.
- Cursor and selection remain document coordinates and are projected through the same materialized range.

### Applied viewport output

Projection should return one resolved output:

```rust
struct AppliedTranscriptViewport {
    materialized_rows: crate::smelt_edit::MaterializedRows,
    top_anchor: Option<TranscriptScrollAnchor>,
    scrollbar_total_rows: RowIndex,
    exact_visible_range: Range<RowIndex>,
    placeholder_rows_visible: bool,
}
```

Rules:

- The render loop applies this output to `Window`.
- Tests should inspect this output or equivalent trace data.
- Placeholder visibility is allowed only for far sparse gaps, never as a side effect of nearby scroll.

### Level 1: Exact materialized rows

Scope:

- visible viewport;
- bounded overscan;
- selection endpoints;
- copied/yanked ranges;
- search hit refinement;
- fold/action/link hit testing;
- active cursor row;
- local wheel/autoscroll while the needed rows are in or near the loaded window.

Rules:

- Never use estimates for text, copy, selection, actions, search hit rows, or hit testing.
- Materialize only the needed descriptor rows and bounded overscan.
- Use existing `TranscriptHeightIndex` exact heights when available.
- Exact row data may be cached in memory by `(descriptor/block id, width, renderer generation, presentation generation, view state generation)`.

### Level 2: Exact cached descriptor heights

Scope:

- loaded descriptors that have already been rendered or measured for the current width/view state;
- nearby scroll and autoscroll after initial exactification;
- preserving anchors across local re-render, append, resize, theme change, and group/fold state changes.

Rules:

- Exact cached heights can replace estimates for loaded descriptors.
- Cache invalidation must be keyed by all state that changes row height: terminal width, renderer generation, renderer cache key, presentation generation, fold/group view state, and descriptor content hash.
- Exact height cache is optional and lazy. Absence of exact cache must not trigger full transcript layout.

### Level 3: Stable unloaded descriptor estimates

Scope:

- scrollbar total extent;
- coarse scrollbar click/drag landing;
- choosing the next descriptor window for sparse scroll beyond the loaded window;
- mapping a far numeric row request into an approximate descriptor range before local refinement.

Rules:

- Estimates must be stable for a descriptor range and width. They must not depend on the current loaded window.
- Estimates are allowed to change only when the underlying descriptor metadata or estimate algorithm version changes, not when scrolling loads new windows.
- Estimate refinement must preserve visible content through semantic anchors. It may move the scrollbar thumb slightly, but not the viewport content unexpectedly.
- Estimate queries must be cheap enough for render-time use or moved off the render path.

### Level 4: Compatibility fallback estimate

Scope:

- legacy or partially migrated sessions that genuinely lack sparse descriptor metadata;
- explicit repair/import paths;
- tests that construct sparse documents without a store.

Rules:

- Fallbacks must be named as compatibility or test behavior.
- They must not silently become the normal production path.
- If compatibility code is intended to be removed, tag it with `COMPAT(<id>)` and document it in `docs/compat.md`.

## Target abstractions

### `TranscriptScrollAnchor`

A semantic viewport anchor, not a row number.

Suggested shape:

```rust
struct TranscriptScrollAnchor {
    descriptor_index: usize,
    block_id: Option<BlockId>,
    intra_block_row: RowIndex,
    bias: AnchorBias,
}
```

Uses:

- preserve viewport top across estimate refinement;
- preserve active selection autoscroll intent;
- preserve clicked/dragged content identity;
- resolve `scroll_top` after sparse window replacement;
- keep tail-follow independent from numeric total-row estimate.

`scroll_top` remains useful as a resolved render output and for non-transcript row documents, but transcript sparse scrolling should treat it as derived state.

### `TranscriptExtentIndex`

The single document-owned abstraction for exact and estimated row extents.

Responsibilities:

- answer exact rows for materialized ranges;
- answer cached exact heights for loaded descriptors;
- answer stable estimates for unloaded descriptor ranges;
- map descriptor anchors to approximate rows;
- map approximate rows to descriptor ranges for far jumps;
- expose total estimated rows for scrollbar only;
- track whether returned rows are exact or estimated.

Suggested return type:

```rust
enum RowExtent {
    Exact(RowIndex),
    Estimated(RowIndex),
}
```

Rules:

- Callers must not accidentally use an `Estimated` value where exact rows are required.
- `TranscriptDocument` should be the only production owner that combines exact and estimated extents.
- `transcript_buf.rs` row-index data should either feed this abstraction or be owned by it, instead of remaining an independent parallel model.

### `SparseWindowManager`

A document-owned policy object for descriptor windows.

Responsibilities:

- decide which descriptor range to load for an anchor, nearby scroll, selection autoscroll, search target, or scrollbar jump;
- maintain bounded overscan above and below the viewport;
- avoid replacing the active window in a way that invalidates the current content anchor;
- distinguish local exact scrolling from far estimated seeking.

### `PersistentExtentStore`

A store-facing interface for unloaded estimates.

Responsibilities:

- provide descriptor count and dense descriptor extents without loading descriptor JSON;
- provide stable unloaded row estimates by range and width;
- provide optional chunked or cached summaries so large-range estimates do not scan all descriptors on render paths;
- eventually persist lazy exact-height observations if measurements show it is worth the write complexity.

## Store estimate design

The current SQLite aggregate is correct for stability but not final for performance:

```sql
SUM(((MAX(estimated_text_bytes, 1) + width - 1) / width) + 1)
```

It scans the requested descriptor range. At 128k descriptors this showed about 23 ms in the latest resume benchmark. The final model should remove this from first tail render.

### Recommended store model

Use a two-tier estimate source:

1. **Fast coarse extent from width-independent descriptor metadata**
   - descriptor count;
   - prefix or chunked byte totals;
   - optional per-kind counts if cheap and useful.

2. **Stable per-width chunk summaries**
   - keyed by `(width, estimate_algorithm_version, chunk_index)`;
   - each chunk covers a fixed descriptor span, for example 512 or 1024 descriptors;
   - each row stores the aggregate estimated rows for that chunk at that width;
   - suffix descriptor replacement invalidates affected chunk summaries;
   - first query for a missing chunk can compute only that chunk, not the full unloaded range.

This gives stable estimates with bounded query cost:

```text
range estimate = partial start chunk + full cached chunks + partial end chunk
```

For 128k descriptors and 1024-descriptor chunks, a full-prefix estimate touches about 125 chunk rows instead of 127,920 descriptor rows once chunk summaries exist. Missing summaries can be filled lazily and incrementally.

### Why not exact persisted row heights for everything?

Exact persisted row heights are width and view-state dependent. Groups, collapsed tool calls, renderer changes, markdown rendering, terminal width, fold state, and plugin renderers can all change height. Computing exact heights for all descriptors at every width would require loading and rendering the full transcript, which violates the core invariant.

Persisting exact heights lazily is useful only as a cache:

- write exact height after a descriptor is actually rendered;
- key by width and presentation/view-state generation;
- use it as Level 2 exact cached knowledge;
- never require it before rendering visible content.

### Why not use only `sum(bytes) / width`?

A byte-prefix estimate is very fast and stable, but it loses per-descriptor wrapping effects. It can be acceptable as a temporary coarse fallback for scrollbar position before chunk summaries are available, but it should not be the final only model for heterogeneous transcripts.

The final model can still keep byte-prefix fallback because semantic anchors prevent viewport jitter when estimates refine.

## Operation-specific rules

### Tail resume and tail-follow

Tail resume should not block on estimating the whole unloaded prefix just to render the tail.

Correct behavior:

1. Load tail descriptors only.
2. Render the exact tail viewport and overscan.
3. Set the viewport anchor to tail, not to a computed `scroll_top` that requires exact total rows.
4. Provide an approximate total extent for the scrollbar from the fastest available store estimate.
5. Refine total extent later without moving visible tail content.

Current fit:

- Tail descriptor load is already bounded to 80 descriptors in the latest benchmark.
- Current tail load still calls a large descriptor estimate, visible in `store:transcript:descriptor_estimated_rows last_us=23209`.

Plan implication:

- Move large prefix/suffix estimation out of synchronous tail render.
- Tail-follow should resolve from `TranscriptScrollAnchor::Tail` plus exact visible tail rows.

### Normal wheel scrolling

Nearby wheel scrolling should first be exact and local.

Correct behavior:

1. Scroll by rows inside the current exact materialized window when possible.
2. When nearing the window boundary, load adjacent descriptors with overscan.
3. Preserve viewport top by semantic anchor while replacing or merging descriptor windows.
4. Use unloaded estimates only to decide whether a sparse gap exists and how much scrollbar extent remains.

Current fit:

- The heterogeneous sparse wheel-up regression verifies visible record monotonicity.
- The current numeric `scroll_top` model is smooth after stable estimates, but the final model should make content anchor preservation explicit.

### Active selection autoscroll

Selection autoscroll must be content anchored.

Correct behavior:

1. Selection anchor and active edge live in document coordinates.
2. Edge autoscroll loads adjacent descriptor windows when needed.
3. Visible rows and selected text are exact.
4. Sparse estimates only guide prefetch and scrollbar extent.

### Scrollbar drag and click

Scrollbar interactions are the main legitimate consumer of estimates.

Correct behavior:

1. Map scrollbar fraction to an approximate descriptor index using the extent index.
2. Load a descriptor window around that approximate location.
3. Materialize exact visible rows.
4. Re-anchor to the nearest real descriptor/block row.
5. If refinement changes local height, preserve the content anchor, not the original numeric row.

### Search, copy, folds, actions

These must never return estimated content.

Correct behavior:

- Search uses SQLite candidates plus bounded exact display refinement.
- Copy exactifies the selected row range only.
- Folds and actions hydrate the target descriptor/window exactly before acting.
- Estimated gaps are not selectable text and have no actions.

## Testing and observability model

The existing tests are necessary but not sufficient. They prove many internal invariants, but they do not reproduce the subjective bug report: inconsistent velocity, lag, and occasional perceived snap-back during real gestures.

### Scroll trace instrumentation

Add a transcript scroll trace that can be enabled in tests and optionally during local debugging. Each frame should record:

```text
input_event_or_tick
scroll_intent
window_scroll_before
window_scroll_after_input
viewport_anchor_before
projection_target
active_descriptor_range_before
prefix_estimate_before
suffix_estimate_before
exact_observation_count
resolved_scroll_top
viewport_anchor_after
active_descriptor_range_after
materialized_range
placeholder_rows_visible
first_visible_content_anchor
last_visible_content_anchor
visible_record_or_block_ids
render_or_projection_ms
```

Requirements:

- The trace must be deterministic in tests.
- It must not log user transcript content by default. Use descriptor indices, block ids, row anchors, counts, and timings.
- It must be cheap when disabled.
- It should explain whether a bad frame is caused by semantic drift, variable velocity, descriptor loading, placeholder landing, event coalescing, or render latency.

### Replay tests

Add a scroll-trace replay harness that can run a recorded sequence of intents against a deterministic sparse transcript.

Required scenarios:

1. Repeated wheel up from tail through heterogeneous descriptor heights.
2. Repeated wheel down from top into loaded and unloaded regions.
3. Drag selection to top edge with autoscroll ticks.
4. Drag selection to bottom edge with autoscroll ticks.
5. Wheel bursts with production coalescing semantics.
6. Exact height observations injected mid-gesture.
7. Descriptor-window replacement while preserving visible content.
8. Resize while a wheel or drag gesture is in progress.
9. Streaming append while scrolled away from tail.
10. Scrollbar drag to far sparse positions.

Assertions:

- Visible content anchor never reverses for a monotonic gesture.
- Per-tick content movement stays within a narrow expected range except at real content boundaries.
- Nearby scroll never produces a placeholder-only viewport.
- Projection/render latency stays under a defined budget for local scroll frames.
- Descriptor JSON loads and materialized row counts stay bounded.
- Scrollbar extent may refine, but the visible anchor does not move unexpectedly.

### End-to-end test gap to close

The harness must exercise the real path, not just direct document calls:

```text
terminal event -> wheel coalescing or drag tick -> Ui -> Window geometry -> TranscriptDocument intent -> projection -> Window applied viewport -> rendered frame snapshot
```

Any direct `TranscriptDocument` unit test must be paired with at least one full-frame harness test if it protects user-visible scrolling behavior.

## Cleanup requirements

Cleanup is part of the architecture work, not a final polish step.

Delete or simplify the following once the new model owns the behavior:

1. **Transcript-specific row reinterpretation in `Window`.**
   Keep generic row-document painting and geometry, but remove transcript sparse semantics from `Window` paths.

2. **Patch-level semantic-anchor fallbacks.**
   Remove local conditions that try to repair `ExactRow(scroll_top)` after the fact. The correct model should pass explicit intents and semantic viewport state before numeric rows are produced.

3. **Duplicate extent calculations.**
   Consolidate loaded exact heights, unloaded estimates, mixed totals, prefix/suffix math, and scrollbar totals under `TranscriptExtentIndex` or its replacement.

4. **Approximate APIs used by exact consumers.**
   Delete or rename any method that can silently provide estimated rows to copy, hit testing, selection, actions, cursor placement, or visible content projection.

5. **Obsolete tests that assert implementation artifacts.**
   Keep tests that assert user-visible contracts and bounded work. Remove or rewrite tests that only assert the old numeric-row workflow.

6. **Temporary tracing or debug output.**
   Trace infrastructure may stay behind a test/debug gate. Ad hoc logging and one-off metrics added for diagnosis must be removed before final validation.

7. **Dead compatibility or fallback paths.**
   If a fallback is only for tests, make it test-only. If it is for legacy data, tag it with `COMPAT(<id>)` and document removal criteria in `docs/compat.md`.

## Refactor phases

The original six phases are complete but insufficient. They improved boundedness and removed several regressions, but they did not finish the architectural migration away from numeric-row authority. The following phases supersede the earlier phase list and should be implemented in order. Each phase must validate the new user-facing scroll contract, not only internal counters.

### Phase 0: Freeze symptom patches and capture current behavior

Goal: stop adding local repairs before the correct model is observable.

Tasks:

- Do not add more special cases that reinterpret `ExactRow(scroll_top)` after projection has already started.
- Review uncommitted or recent scroll patches and classify each as:
  - keep because it matches the final model;
  - temporary diagnostic aid;
  - remove once intent-based scrolling lands.
- Add a short note near any temporary code that names the final deletion condition. Do not add compatibility shims for unreleased intermediate behavior.
- Record one or more real bad scroll sessions with enough information to identify semantic drift, variable velocity, placeholder landing, event coalescing, or render latency.

Acceptance:

- The plan and code review identify which current fixes are architectural and which are symptom patches.
- No new behavior change is made without a trace or replay that explains the problem.

Phase 0 audit record:

- The uncommitted patch that tried to rebase `ExactRow(scroll_top)` through `viewport_anchor` after projection planning was removed before Phase 0 implementation. It was a symptom patch because it added another local repair to the numeric-row path instead of representing the original user action as a transcript scroll intent.
- The completed Phase 1 to Phase 6 commits are retained as the current scalability baseline:
  - keep `TranscriptExtentIndex` naming and approximate/exact API naming because they align with the final extent-owner direction;
  - keep bounded estimate fallback and exact loaded descriptor observations because they preserve boundedness;
  - treat current `viewport_anchor` repair logic as transitional because it still receives numeric row requests after `Window` and `Ui` have already interpreted the gesture;
  - treat current full-frame monotonic tests as useful but insufficient because they do not assert stable content-space velocity or frame latency.
- Current code seams that still collapse intent too early:
  - app wheel coalescing batches terminal events into a numeric delta and calls `Ui::scroll_at`;
  - `Ui::scroll_at` calls `Window::pan_by_lines`, mutating `Window::scroll_top` before transcript projection sees the gesture;
  - drag autoscroll calls `Window::drag_autoscroll_step`, also mutating `Window::scroll_top` directly;
  - render prep turns `request.scroll_top` into `ScrollTarget::visible_row(request.scroll_top)` for transcript projection;
  - `TranscriptDocument::resolve_exact_scroll_target_from_viewport_anchor` can only repair a subset of numeric-row drift after the fact.
- Current real bad-session evidence comes from user testing after the Phase 6 baseline: wheel scrolling and selection autoscroll still have inconsistent speed, lag, and perceived snap-back. The present tests pass while that experience remains bad, so the missing artifact is a deterministic trace/replay that can classify frames into semantic drift, variable velocity, placeholder landing, event coalescing, or projection latency.
- Phase 0 conclusion: no more local row-rebasing behavior changes should land before Phase 1 trace instrumentation and Phase 2 replay/velocity tests exist. Any future behavior fix must include a failing trace or replay that explains the observed frame sequence.

### Phase 1: Define the transcript scroll contract and trace schema

Goal: make the target behavior testable before changing the model.

Tasks:

- Write the scroll contract in code-facing docs or test helper docs:
  - wheel delta semantics;
  - drag autoscroll semantics;
  - scrollbar drag/click semantics;
  - tail-follow semantics;
  - resize/reflow semantics;
  - estimate-refinement semantics.
- Add a disabled-by-default transcript scroll trace with the fields listed in the testing section.
- Ensure trace records use ids, descriptor indices, anchors, counts, and timings, not transcript text.
- Add helpers to compare visible content movement in content-anchor space.

Acceptance:

- A local reproduction can produce a trace explaining every frame's input, intent, projection target, resolved viewport, descriptor range, placeholder state, and projection time.
- Trace is cheap when disabled and deterministic in tests.

Phase 1 implementation record:

- Added `crates/tui/src/app/transcript_scroll_trace.rs` as the code-facing scroll contract and trace schema. The contract names wheel delta, drag autoscroll, scrollbar far seek, tail-follow, resize/reflow, and estimate-refinement semantics without changing runtime scroll behavior.
- Added a disabled-by-default in-memory transcript scroll trace on `TranscriptDocument`. When enabled, projection frames record input/tick labels, semantic intent labels, window scroll before/after input, viewport anchors, projection targets, descriptor ranges, prefix/suffix estimates, exact observation counts, resolved scroll, materialized ranges, placeholder state, first/last visible content anchors, visible block ids, and optional projection timings.
- Trace records use descriptor indices, row anchors, render node ids, block ids, row counts, and optional timings. They do not record transcript text.
- Added `compare_visible_content_movement` and `TranscriptVisibleContentAnchor` as Phase 2 helpers for content-anchor velocity assertions.
- Wired the render-prep path to seed trace frames from the current row-based architecture only when tracing is enabled, keeping disabled tracing on a cheap `Option::is_some` branch.
- Added a deterministic unit test for trace frame fields with timings disabled.

Phase 1 validation:

- `cargo fmt`
- `cargo test -p smelt-tui --features harness transcript_scroll_trace`
- `cargo test -p smelt-tui --features harness app::transcript::document_tests`
- `cargo clippy -p smelt-tui --all-targets --features harness -- -D warnings`

### Phase 2: Build replay and velocity tests that fail for the real bug class

Goal: test the experience the user reports, not only numeric monotonicity.

Tasks:

- Add a scroll replay harness that drives the full event path where possible.
- Add deterministic sparse transcript fixtures with heterogeneous descriptor heights.
- Cover wheel bursts, normal wheel ticks, top and bottom drag-autoscroll, exact height refinement mid-gesture, descriptor-window replacement, resize during scroll, streaming append while pinned, and scrollbar far seek.
- Add velocity assertions over visible content anchors.
- Add latency assertions for local scroll projection frames.

Acceptance:

- At least one replay or full-frame test fails on the current architecture for the observed lag/jitter class before the model rewrite.
- Passing tests must prove stable content movement and bounded latency, not merely nondecreasing `scroll_top`.

Phase 2 implementation record:

- Added a transcript scroll replay helper in the harness tests that drives the real app path for wheel ticks, coalesced wheel deltas, drag-autoscroll ticks, resize/reflow, streaming append while pinned, descriptor-window replacement, and scrollbar far seek.
- Added deterministic heterogeneous sparse transcript replay coverage using trace frames, visible content anchors, descriptor ranges, placeholder flags, and optional projection timings.
- Added content-anchor assertions for local upward and downward movement, local placeholder rejection, and projection latency budgets. The assertions inspect trace anchors and block ids, not only `scroll_top`.
- Added a direct trace test for an injected exact-height observation count so replay assertions can classify frames that run after extent refinement.
- Added an ignored failing full-frame replay test, `transcript_scroll_replay_requires_wheel_intent_before_numeric_row_target`, that demonstrates the current architecture still collapses a wheel tick into `CurrentRowTarget(row)` instead of delivering `UserDelta { rows: -3 }` to transcript projection. This is the Phase 3 red test.

Phase 2 validation:

- `cargo fmt`
- `cargo test -p smelt-tui --features harness transcript_scroll`
- `cargo clippy -p smelt-tui --all-targets --features harness -- -D warnings`
- Explicit red-test check: `cargo test -p smelt-tui --features harness transcript_scroll_replay_requires_wheel_intent_before_numeric_row_target -- --ignored --nocapture` fails with `CurrentRowTarget(...)` vs `UserDelta { rows: -3 }`.

### Phase 3: Introduce explicit `TranscriptScrollIntent`

Goal: stop collapsing distinct user actions into numeric row requests before transcript projection sees them.

Tasks:

- Add `TranscriptScrollIntent` with variants for tail, preserve viewport, user delta, page delta, semantic anchor jump, resize reflow, scrollbar fraction, and approximate row seek.
- Route wheel and drag-autoscroll paths to produce `UserDelta` for transcript windows.
- Route scrollbar paths to produce `ScrollbarFraction` or `ApproximateRowSeek`.
- Route search/reveal to semantic anchors.
- Keep existing behavior behind an adapter only long enough to compare traces.

Acceptance:

- Trace shows transcript projection receiving the original user intent.
- App/UI/Window code no longer needs transcript-specific sparse row interpretation to express user actions.

Phase 3 implementation record:

- Kept `TranscriptScrollIntent` as the trace-facing contract vocabulary and added production adapters at the app/UI boundary so transcript mouse wheel, coalesced wheel, drag autoscroll, scrollbar click/drag, and search reveal paths seed trace input with semantic intent before the render projection frame.
- Added `Ui::drag_autoscroll_delta` so the app can observe the edge-drag owner and row delta before the generic UI mutates `Window::scroll_top`.
- Routed transcript wheel and drag-autoscroll paths to `UserDelta`, scrollbar paths to `ScrollbarFraction` with an `ApproximateRowSeek` fallback, and search jumps to `SearchJump(Content { ... })` when the target row has a content anchor.
- Added `TranscriptDocument::trace_anchor_at_row` to expose a trace-safe semantic anchor lookup without leaking the private viewport-anchor representation.
- Converted the Phase 2 ignored wheel-intent red test into a normal regression and added production-path coverage for scrollbar fraction intents and search-jump semantic anchors.
- The row-based `Window` mutations remain as the temporary adapter for behavior comparison. Phase 4 moves durable viewport state into `TranscriptDocument` so these intents become projection inputs instead of trace-only evidence.

Phase 3 validation:

- `cargo fmt`
- `cargo test -p smelt-tui --features harness transcript_scroll -- --nocapture`
- `cargo test -p smelt-tui --features harness preserves_ -- --nocapture`
- `cargo clippy -p smelt-tui --all-targets --features harness -- -D warnings`
- `cargo nextest run --workspace --features smelt-tui/harness`

### Phase 4: Make `TranscriptDocument` own durable viewport state

Goal: make `Window::scroll_top` derived for transcript windows.

Tasks:

- Add `TranscriptViewportState` to `TranscriptDocument` or the document-owned view state.
- Store semantic top anchor, row offset, mode, and pending intent.
- Resolve pending intents during transcript projection against exact materialized rows and stable sparse extents.
- Return `AppliedTranscriptViewport` with materialized rows, resolved scroll, visible anchors, scrollbar extent, and placeholder status.
- Update the render loop so `Window` applies the resolved output but does not own transcript scroll semantics.

Acceptance:

- Estimate refinement, exact height observation, and descriptor-window replacement cannot move visible content unless the pending intent asks to move.
- `Window::scroll_top` remains correct for painting and scrollbars but is not the durable transcript position.

Phase 4 implementation record:

- Added `TranscriptViewportState` to `TranscriptDocument` with a semantic top anchor, row offset, viewport mode, pending scroll intent, and the latest resolved paint row.
- Added `TranscriptViewportProjectionInput` and `AppliedTranscriptViewport` so the render loop passes geometry and legacy paint rows into the document, then applies only the resolved materialized rows, scrollbar extent, visible range, placeholder status, and tail-follow output back to `Window`.
- Moved pending-intent resolution into transcript projection. Tail, preserve, resize, user/page deltas, content jumps, search jumps, scrollbar fraction seeks, approximate seeks, and current-row adapters now resolve inside `TranscriptDocument` against exact materialized anchors or stable sparse estimates.
- Captured the top visible anchor after each projection so later preserve/delta projections use document-owned semantic state instead of reinterpreting `Window::scroll_top`.
- Kept one named boundary adapter, `COMPAT(transcript-window-scroll-top-adapter)`, for tests and paths that still mutate `Window::scroll_top` directly. Phase 7 removes this after transcript row-authority writers are migrated to intents.
- Added `transcript_viewport_state_preserves_anchor_without_window_scroll_change` to assert that a render with no new window scroll mutation preserves the document-owned top anchor and records `PreserveViewport` rather than an exact row target.

Phase 4 validation:

- `cargo fmt`
- `cargo test -p smelt-tui --features harness transcript_scroll -- --nocapture`
- `cargo test -p smelt-tui --features harness transcript_ -- --nocapture`
- `cargo clippy -p smelt-tui --all-targets --features harness -- -D warnings`
- `cargo nextest run --workspace --features smelt-tui/harness`

### Phase 5: Rework local scrolling and autoscroll around exact content movement

Goal: make nearby scrolling consistent and exact.

Tasks:

- Resolve `UserDelta` inside the current exact materialized/overscan range when possible.
- When the delta approaches a window boundary, load adjacent descriptors before landing in placeholders.
- Keep drag selection anchor and active edge in document coordinates.
- Define what happens at real content boundaries and when adjacent descriptors cannot be loaded.
- Ensure wheel coalescing produces a single intent with a known row delta, not several partially-applied row assignments.

Acceptance:

- Repeated wheel and autoscroll ticks move visible content at a stable rate in replay tests.
- Nearby scroll does not produce placeholder-only viewports.
- Selection autoscroll keeps growing selection exactly while maintaining bounded materialization.

Phase 5 implementation record:

- Changed `UserDelta` and `PageDelta` resolution to target reflow-stable rows from the document-owned viewport anchor instead of plain exact numeric rows, so exact height refinement preserves the requested content identity during local movement.
- Local movement after tail-follow now starts from the last document-resolved paint row, not the generic `Window` row after pre-scroll, preventing wheel and autoscroll deltas from being applied twice.
- Local delta projection activates descriptor windows around the requested row before planning, so nearby wheel/page movement loads adjacent descriptor content instead of landing in sparse placeholders.
- Pending `UserDelta` intents coalesce inside `TranscriptDocument`, preserving one accumulated content-row movement when multiple local scroll inputs arrive before projection.
- Updated the replay harness so coalesced wheel and drag-autoscroll scenarios use the production transcript-intent adapters, then tightened replay assertions to require reflow-stable local targets, exact visible anchors, no local placeholders, and separated monotonic checks for contiguous upward sequences.
- Added `transcript_viewport_state_coalesces_pending_user_deltas` to cover accumulated document-owned deltas and their reflow-stable projection target.

Phase 5 validation:

- `cargo fmt`
- `cargo test -p smelt-tui --features harness transcript_viewport_state_coalesces_pending_user_deltas -- --nocapture`
- `cargo test -p smelt-tui --features harness transcript_scroll_replay_covers_velocity_latency_and_sparse_scenarios -- --nocapture`
- `cargo test -p smelt-tui --features harness transcript_scroll -- --nocapture`
- `cargo test -p smelt-tui --features harness transcript_ -- --nocapture`
- `cargo clippy -p smelt-tui --all-targets --features harness -- -D warnings`
- `cargo nextest run --workspace --features smelt-tui/harness`

### Phase 6: Restrict estimates to scrollbar and far seek

Goal: prevent estimates from influencing visible content identity or local scroll velocity.

Tasks:

- Audit every use of approximate total rows, sparse prefix/suffix rows, mixed totals, and unloaded gap rows.
- Split APIs by precision and intent:
  - exact visible materialization;
  - exact loaded descriptor extent;
  - approximate scrollbar total;
  - approximate far seek;
  - compatibility/test fallback.
- Ensure unloaded sparse gaps are inert and reachable only through intentional far seeks or insufficiently loaded far scrollbar targets.
- Keep or improve the bounded estimate-store behavior from the first six phases.

Acceptance:

- Exact consumers cannot call approximate APIs by accident.
- Estimates affect scrollbar geometry and far seek only.
- Copy, hit testing, selection, actions, folds, cursor placement, and visible local scroll use exact rows.

Phase 6 implementation record:

- Audited sparse estimate consumers and kept approximate totals on scrollbar geometry, far seek targeting, trace metadata, descriptor activation, and compatibility/test projection entry points.
- Split viewport projection planning so production transcript intents decide whether sparse placeholders may be materialized. `UserDelta`, `PageDelta`, `PreserveViewport`, `ResizeReflow`, semantic content/search, and `Tail` intents now disallow placeholder rows; scrollbar fraction, approximate row seek, and the temporary current-row compatibility target may still use inert sparse placeholders.
- Replaced exact content-row lookup through sparse prefix estimates with an exact loaded virtual-row span. Node metadata, row anchors, folds, and exact block-node test coverage now reject unloaded prefix and suffix rows instead of translating them through estimates.
- Preserved full-transcript behavior by treating non-sparse documents as an exact loaded span from row zero, and kept intentional loaded-window backward searches exact within each loaded descriptor window.
- Strengthened sparse-gap tests so unloaded prefix and suffix rows expose no text, actions, metadata, row anchors, block nodes, or fold targets. Added a viewport test proving local deltas stay on exact loaded content while far seeks can still expose inert placeholders.

Phase 6 validation:

- `cargo fmt`
- `cargo test -p smelt-tui --features harness unloaded_sparse_gaps_do_not_provide_content_or_hit_targets -- --nocapture`
- `cargo test -p smelt-tui --features harness local_delta_without_adjacent_descriptors_stays_on_exact_loaded_content -- --nocapture`
- `cargo test -p smelt-tui --features harness sparse_block_lookup_searches_previous_windows_for_role_match -- --nocapture`
- `cargo test -p smelt-tui --features harness transcript_scroll -- --nocapture`
- `cargo test -p smelt-tui --features harness transcript_ -- --nocapture`
- `cargo clippy -p smelt-tui --all-targets --features harness -- -D warnings`
- `cargo nextest run --workspace --features smelt-tui/harness`

### Phase 7: Cleanup obsolete row-authority paths

Goal: finish with fewer competing models than before.

Tasks:

- Remove temporary adapters that convert transcript intents back into `ExactRow(scroll_top)` too early.
- Remove patch-level anchor fallbacks made obsolete by durable viewport state.
- Remove transcript-specific sparse behavior from `Window` except resolved paint geometry.
- Consolidate exact loaded heights and sparse estimates under one extent owner.
- Delete stale tests that assert the old numeric-row workflow. Replace them with contract, trace, and replay tests.
- Remove ad hoc tracing/debug metrics. Keep only structured trace infrastructure behind test/debug gates.
- Tag any real legacy compatibility fallback with `COMPAT(<id>)` and document it in `docs/compat.md`.

Acceptance:

- There is one owner for transcript viewport state and one owner for transcript extent knowledge.
- The code no longer needs local row-rebasing patches to make sparse scrolling feel correct.
- The cleanup diff removes more old row-authority code than it adds in replacement glue.

Phase 7 implementation record:

- Removed `TranscriptScrollIntent::CurrentRowTarget`, the `COMPAT(transcript-window-scroll-top-adapter)` adapter, and the corresponding `docs/compat.md` entry so transcript projection no longer infers semantic intent from a changed `Window::scroll_top`.
- Removed `TranscriptDocument::resolve_exact_scroll_target_from_viewport_anchor`, the patch-level row repair helper made obsolete by document-owned viewport state and explicit intents.
- Routed remaining transcript document-command scroll changes through `ExactContentAnchor` intents before render, and made transcript test seeks record `ApproximateRowSeek` explicitly before mutating the resolved paint row.
- Replaced the stale numeric-row replay assertion with intent-contract coverage that verifies wheel deltas preserve `UserDelta` semantics and do not expose local sparse placeholders.
- Kept generic `Window` row APIs for non-transcript documents and resolved paint geometry, while removing transcript-specific row-authority interpretation from the transcript document boundary.

Phase 7 validation:

- `cargo test -p smelt-tui --features harness transcript_scroll -- --nocapture`
- `cargo test -p smelt-tui --features harness viewport_content_anchor_survives_sparse_prefix_estimate_refinement -- --nocapture`
- `cargo test -p smelt-tui --features harness transcript_vim_visual_char_starts_at_cursor -- --nocapture`
- `cargo test -p smelt-tui --features harness transcript_ -- --nocapture`
- `cargo fmt`
- `cargo clippy -p smelt-tui --all-targets --features harness -- -D warnings`
- `cargo nextest run --workspace --features smelt-tui/harness`

### Phase 8: Benchmark, validate, and update this plan

Goal: prove the correct model is both smoother and still bounded.

Tasks:

- Run targeted replay tests and full transcript tests.
- Run workspace tests and clippy.
- Run the large transcript benchmark with `TMPDIR=/home/dev/tmp`.
- Compare scroll latency, descriptor loads, materialized rows, full row builds, and search/copy boundedness to the Phase 6 baseline.
- Update this document with final results and any deliberate tradeoffs.

Acceptance:

- No full transcript load on normal hot paths.
- First tail render remains bounded and does not scan full unloaded prefixes.
- Wheel and drag-autoscroll replay tests prove stable velocity in content space.
- Local scroll projection latency stays within the chosen budget.
- Placeholder-only viewports are limited to intentional far sparse seeks.
- Code ownership is simpler: transcript scroll semantics live in `TranscriptDocument`, not split across app, UI, window, projection, and extent helpers.

Phase 8 implementation record:

- Reran the targeted transcript scroll replay tests and full transcript test filter after Phase 7 cleanup to verify the intent-owned viewport model still protects stable content movement, local placeholder rejection, semantic search jumps, scrollbar fraction intents, and transcript document commands.
- Reran workspace formatting, clippy, and nextest to validate the completed scroll model across the full workspace.
- Reran the large transcript benchmark with `TMPDIR=/home/dev/tmp` and captured output at `/home/dev/tmp/smelt-transcript-scroll-model-bench-phase8.txt`.
- Final benchmark results stayed within the established bounded-work gates:
  - 10 MiB mixed layout: `first_ms=15.819`, `resize_ms=3.251`, `theme_ms=3.100`, `scroll12_ms=20.461`, `visible_ms=3.304`, `copy_ms=14.097`, `append_ms=7.231`, with `full_row_builds=0` for first, resize, theme, scroll12, visible, copy, append, and no-cache passes.
  - 500 MiB search/view: `width_resize_ms=4.087`, `height_resize_ms=3.485`, `theme_color_ms=2.852`, `copy_mid_ms=0.307`, `nav_ctrl_d20_ms=38.992`, `nav_ctrl_u20_ms=29.636`, `nav_gg_ms=0.567`, `nav_G_ms=5.083`, `rare_ms=26.979`, `common_submit_ms=11.730`, `next100_ms=99.369`, `after_append_ms=14.776`.
  - After-append search stayed bounded with `dirty_candidate_blocks=1`, `dirty_candidates_scanned=1`, `transcript:collect_nodes_range:rows total=40`, and `transcript:render_plan:reused last=1`.
  - 500 MiB descriptor-backed resume stayed bounded with `descriptors=128000`, `descriptor_slice_requested=80`, `descriptors_loaded=80`, `descriptor_json_bytes_loaded=331120`, `tail_load_ms=36.031`, and `tail_render_ms=1.266`.
  - No `store:transcript:descriptor_estimated_rows` metric was emitted, so first tail render did not synchronously scan the unloaded descriptor prefix for row estimates.

Phase 8 validation:

- `cargo test -p smelt-tui --features harness transcript_scroll -- --nocapture`
- `cargo test -p smelt-tui --features harness transcript_ -- --nocapture`
- `cargo fmt`
- `cargo clippy --workspace --all-targets --features smelt-tui/harness -- -D warnings`
- `cargo nextest run --workspace --features smelt-tui/harness`
- `TMPDIR=/home/dev/tmp cargo xtask bench-transcript-layout --runs 1 --workloads mixed_10mib --search --search-bytes 524288000 --resume --resume-bytes 524288000 --no-warmup`

## Historical Phase 6 results

The original six phases were implemented and validated. They are now considered a scalability baseline, not the final scroll model. This section records that baseline and the tradeoffs left by the incomplete model.

The full benchmark command was rerun with home temp storage:

```bash
mkdir -p /home/dev/tmp
TMPDIR=/home/dev/tmp cargo xtask bench-transcript-layout \
  --runs 1 \
  --workloads mixed_10mib \
  --search \
  --search-bytes 524288000 \
  --resume \
  --resume-bytes 524288000 \
  --no-warmup
```

Output was captured at:

```text
/home/dev/tmp/smelt-transcript-scroll-model-bench-phase6.txt
```

### Final 10 MiB mixed layout workload

```text
TRANSCRIPT_LAYOUT_BENCH_SAMPLE workload=mixed_10mib run=1 input_bytes=10497943 generated_bytes=10499021 blocks=3404 rows=141762 first_ms=15.582 resize_ms=3.243 theme_ms=3.104 scroll12_ms=20.583 visible_ms=3.280 copy_ms=13.613 append_ms=7.127 no_cache_ms=15.045 allocs=35188 bytes_allocated=63187803
```

The `TRANSCRIPT_LAYOUT_COUNTERS_JSON` gate stayed at `full_row_builds=0` for first, resize, theme, scroll12, visible, copy, and append.

### Final 500 MiB search/view workload

```text
TRANSCRIPT_SEARCH_BENCH_SAMPLE run=1 bytes=524290206 rows=6413965 width_resize_ms=3.735 height_resize_ms=3.416 theme_color_ms=2.661 copy_mid_ms=0.292 nav_ctrl_d20_ms=26.220 nav_ctrl_u20_ms=19.486 nav_gg_ms=0.534 nav_G_ms=4.696 rare_ms=25.355 common_submit_ms=6.726 next100_ms=56.131 after_append_ms=12.066
```

Important bounded after-append counters:

```text
search:transcript:dirty_candidate_blocks last=1
search:transcript:dirty_candidates_scanned last=1
transcript:collect_nodes_range:rows total=42
transcript:render_plan:reused last=1
```

### Final 500 MiB descriptor-backed resume workload

```text
TRANSCRIPT_TRUE_RESUME_SAMPLE mode=descriptor_backed target_bytes=524288000 generated_bytes=524288000 descriptors=128000 rows=7551997 setup_ms=16411.351 tail_load_ms=42.738 tail_render_ms=1.270
```

Important bounded resume counters:

```text
store:transcript:descriptor_slice_requested last=80
store:transcript:descriptors_loaded last=80
store:transcript:descriptor_json_bytes_loaded last=331120
```

No `store:transcript:descriptor_estimated_rows_requested` metric was emitted in this run, so first tail render did not synchronously scan the unloaded descriptor prefix for row estimates.

### Final tradeoffs

- `TranscriptDocument` now owns the sparse extent boundary, semantic viewport anchor, exact loaded descriptor observations, and approximate scrollbar extent naming.
- `transcript_buf.rs` still owns render-plan reuse and exactification internals. It exposes exact height snapshots to the document extent index rather than becoming a second sparse extent model.
- Large unloaded estimate scans are replaced on render paths by bounded coarse fallback. This keeps first tail render fast while preserving content anchors if estimates refine later.
- Unloaded sparse gaps materialize only inert placeholder rows. They do not provide copy text, actions, node metadata, or fold targets.

## Benchmark gates for future changes

Always run large transcript benchmarks with home temp storage:

```bash
mkdir -p /home/dev/tmp
TMPDIR=/home/dev/tmp cargo xtask bench-transcript-layout \
  --runs 1 \
  --workloads mixed_10mib \
  --search \
  --search-bytes 524288000 \
  --resume \
  --resume-bytes 524288000 \
  --no-warmup
```

Do not tee benchmark output to `/tmp`. If output needs to be captured, use `/home/dev/tmp/...`.

Minimum gates:

- descriptor-backed resume loads a bounded descriptor window;
- first tail render does not load full descriptor JSON;
- synchronous first tail render does not scan all unloaded descriptors for row estimates;
- layout benchmark keeps full row builds at zero for first/resize/theme/scroll/visible/copy/append;
- search/view benchmark keeps after-append search bounded to dirty candidates and render-plan reuse;
- copy/yank remains proportional to selected rows;
- scrollbar estimates may refine, but content anchors must not move unexpectedly.

## Tradeoffs and decisions

### Clear decisions

1. **Do not load the full transcript for exact global height.**
   This violates the non-negotiable memory and resume constraints.

2. **Do not use loaded-window averages for sparse gaps.**
   They are fast but unstable and caused the jitter.

3. **Do not make exact row heights mandatory for unloaded descriptors.**
   Exact height is width/view-state/plugin dependent and would require full rendering.

4. **Use semantic anchors before estimate refinement.**
   This is required so any future estimate improvements cannot move visible content.

5. **Move large estimate aggregation off the render path.**
   The latest benchmark shows the current aggregate is a correctness bridge, not the final hot-path model.

### Deliberate small complexity increase

Chunked per-width summaries add storage and invalidation complexity, but they replace O(total descriptors) estimate scans with bounded chunk work while preserving stable estimates. This is a reasonable architectural tradeoff because it keeps performance and consistency without loading full transcript content.

### No unresolved user decision

There is no unclear product tradeoff at this point. The direction is to preserve exactness for visible/semantic operations, use stable estimates only for unloaded global extent and coarse seek, and make those estimates cheap enough to stay out of visible render latency.
