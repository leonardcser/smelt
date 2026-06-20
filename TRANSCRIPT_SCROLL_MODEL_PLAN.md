# Transcript Scroll Model and Sparse Row Extent Plan

## Purpose

This plan refines the transcript virtualization work around one specific question: how scrolling should map between user-visible content, virtual document rows, sparse descriptor windows, and scrollbar position without loading the full transcript.

The current branch is close to smooth in practice after stabilizing sparse prefix/suffix row estimates, but the architecture should not keep estimation as a hidden dependency in places where exact information is available. The final model should make estimation explicit, bounded, and replaceable by exact knowledge whenever exact knowledge is cheap.

This plan does not implement code. It records the target model, the current code seams, benchmark baseline, and the refactor phases needed to finish the transcript scrolling architecture without leaving debt.

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

The result is smooth in practice because loading a new descriptor window no longer changes the estimate for unrelated unloaded ranges based on local content mix.

### What is still architecturally wrong

The current fix is still a transitional model:

1. Estimate lookup is wired directly into `TranscriptDocument` as ad hoc range math instead of through a row-extent abstraction.
2. `virtual_total_rows` can require large unloaded-prefix/suffix estimates even for operations that only need exact tail or exact visible rows.
3. The store aggregate scans descriptor rows for large ranges. In the latest 500 MiB resume benchmark, one estimate over 127,920 unloaded descriptors took about 23 ms and contributed to a 45 ms tail load.
4. Numeric `scroll_top` still carries too much semantic meaning. It should be derived from a content anchor and current extent index, not treated as the durable scroll identity through sparse refinement.
5. There are two related but separate models: loaded render-plan row indexing in `transcript_buf.rs` and sparse unloaded-gap estimation in `app/transcript.rs`. They should be coordinated by one owner.

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

## Refactor phases

### Phase 1: Name the row-extent boundary

Goal: introduce the final abstraction without changing behavior.

Tasks:

- Add a `TranscriptExtentIndex` or equivalent private module under `TranscriptDocument`.
- Move `DescriptorRowsEstimateKey`, `descriptor_rows_estimate_cache`, `estimated_descriptor_rows_for_range`, `missing_descriptor_rows`, `sparse_prefix_row_offset`, `sparse_suffix_rows`, and `virtual_total_rows` behind it.
- Make every method state whether it returns exact, estimated, or mixed row counts.
- Keep current tests passing.

Acceptance:

- No caller outside `TranscriptDocument` does ad hoc sparse prefix/suffix estimate math.
- Exact-required code paths cannot accidentally call an estimated-row method without the type or method name making that obvious.

### Phase 2: Add semantic scroll anchors

Goal: stop treating numeric `scroll_top` as the durable identity of sparse transcript position.

Tasks:

- Add a transcript-specific viewport anchor held by `TranscriptDocument` or document-view state.
- Capture viewport top anchor after each successful projection.
- Resolve anchor to numeric `scroll_top` for `Window` only after descriptor window materialization.
- Preserve anchor across descriptor window switches, estimate refinement, resize, group/fold changes, and streaming append.
- Keep tail-follow as an anchor state, not as a requirement to compute full total rows before rendering.

Acceptance:

- Existing sparse wheel, drag autoscroll, resize anchor, and streaming snap-back tests pass.
- Add a focused test where an estimate changes or is refined while a content anchor remains visible.

### Phase 3: Remove synchronous large-range estimate scans from render paths

Goal: keep stability without paying O(total descriptors) during first tail render or ordinary scroll frames.

Tasks:

- Add `PersistentExtentStore` methods for cheap descriptor count and chunked estimates.
- Avoid opening a fresh read-only SQLite connection per estimate miss. Use a document-owned or app-provided read-only store handle where safe.
- Introduce chunked per-width estimate summaries or an equivalent bounded query mechanism.
- For missing summaries, use stable coarse fallback and schedule or lazily compute chunk summaries, preserving anchors on refinement.

Acceptance:

- 500 MiB descriptor-backed resume loads only the tail descriptor window and does not scan the full unloaded prefix on the synchronous tail-render path.
- Benchmark counters show descriptor JSON loaded remains bounded to the active window.
- `store:transcript:descriptor_estimated_rows_requested` is either absent from first tail render or bounded to chunk-size work.

### Phase 4: Consolidate loaded exact height index with sparse extent model

Goal: stop maintaining two mental models for row heights.

Tasks:

- Treat `TranscriptHeightIndex` exact measurements as Level 1/2 entries in the document extent index.
- Keep render-plan reuse and exactification behavior from `transcript_buf.rs`, but expose it through the document-owned extent API.
- Make exact cached descriptor heights override estimates for loaded descriptors.
- Ensure cache invalidation includes width, renderer generation, renderer cache key, presentation generation, content hash, and view/fold state.

Acceptance:

- Local scroll, copy, search, resize, and theme operations keep bounded counters.
- The 10 MiB layout workload keeps `full_row_builds=0` for first, resize, theme, scroll, visible, copy, and append.

### Phase 5: Restrict estimates to legitimate consumers

Goal: make it hard to use estimates when exact data is available.

Tasks:

- Audit every caller of total rows, materialized rows, block layout, search layout, and copy layout.
- Split APIs by intent:
  - exact materialization;
  - approximate scrollbar extent;
  - approximate far seek;
  - compatibility fallback.
- Rename retained expensive or approximate APIs so call sites reveal cost and precision.
- Add assertions or tests that estimated gaps cannot provide copy text, actions, or hit targets.

Acceptance:

- Estimation is used only for unloaded gaps, global scrollbar extent, and coarse far seeking.
- Visible viewport and overscan are exact.
- No app/window code computes transcript-specific sparse estimates directly.

### Phase 6: Benchmark and simplify

Goal: finish with a simpler architecture, not an extra abstraction layer over the old one.

Tasks:

- Rerun the benchmark command from this plan with `TMPDIR=/home/dev/tmp`.
- Rerun targeted sparse/harness tests, clippy, and workspace tests.
- Compare against the latest baseline in this document.
- Delete any old estimate helpers, caches, or fallbacks made obsolete by the new extent owner.
- Update this plan with final measured numbers and any deliberate tradeoffs.

Acceptance:

- No full transcript load on normal hot paths.
- First tail render is not blocked by a full-prefix descriptor estimate scan.
- Smooth heterogeneous wheel scroll and active selection autoscroll remain covered by full-frame tests.
- Streaming tool and compaction updates do not re-enable tail-follow when the user is scrolled away.
- Code ownership is simpler: `TranscriptDocument` owns transcript row extent and sparse loading policy.

## Final Phase 6 results

All six phases are implemented. This section records the final validation run and the tradeoffs intentionally left in the completed scroll model.

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
