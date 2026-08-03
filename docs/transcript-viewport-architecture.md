# Transcript viewport architecture

## Status

This document defines the architecture and acceptance contract for transcript rendering, resume, and scrolling. Global estimated display rows are metadata; viewport authority comes from a semantic anchor and a bounded exact row tape.

The implementation maps these concepts onto the existing transcript pipeline:

- `TranscriptViewportState` owns the semantic viewport mode and a `TranscriptResolvedViewportAnchor` that keeps the top identity, row offset, and committed numeric consistency check structurally aligned.
- `TranscriptExactViewport` stores an opaque exact-tape handle plus sparse record-coordinate mapping.
- `TranscriptProjection` owns the bounded rendered rows, exact row identities, and generation checks that validate the handle.
- `AppliedTranscriptViewport` is the coherent frame passed to the window adapter.

The persistence format, SQLite record store, logical block types, and individual renderers remain reusable. The component names below describe authority boundaries rather than requiring separate Rust types.

## Problem

A transcript is a sparse virtual sequence:

- Most persisted record bodies may be unloaded.
- Presentation can combine logical blocks into render nodes.
- Rendered height depends on terminal width, theme, renderer generation, folds, and inline presentation options.
- Some loaded nodes have exact heights while unloaded or unmeasured nodes only have estimates.
- The viewport must remain stable while records are paged, bodies are hydrated, layouts are measured, and estimates are refined.

The old pipeline allowed both sparse record paging and rendered-row measurement to mutate the meaning of one global row coordinate. A local input could change the active record range, installed block set, render plan, height estimates, exact observations, sparse prefix, materialized row base, and window scroll coordinate in one frame.

That violated the core user-facing contract. A numeric scroll value could move while painted content remained fixed, or move by three while visible content jumped by multiple records.

## Acceptance contract

### Exact local movement

For a local upward movement of `N` exact display rows, with unchanged content and layout:

```text
after[N..viewport_height] == before[0..viewport_height-N]
```

Downward movement has the symmetric invariant.

This must hold:

- On the first input after a cold render.
- On the first input after resuming a saved session.
- While crossing record pages.
- While crossing render-node and hydration boundaries.
- Across long heterogeneous transcripts.
- With grouped tools, folds, markdown, wide characters, and giant blocks.

### Stable viewport

Paging, hydration, layout measurement, cache eviction, or extent refinement must not move a pinned viewport. Only these events may intentionally change visible content:

- A user navigation intent.
- A content mutation affecting visible presentation.
- A layout-key change such as terminal resize or renderer configuration change.
- Entering or leaving explicit tail-follow mode.

### Bounded work and memory

- Resume loads only enough metadata and bodies to render the tail viewport and bounded overscan.
- Local movement performs work proportional to the movement and newly entered nodes, not total transcript size.
- Materialized rows, record pages, hydrated bodies, and layouts all have explicit memory budgets.
- Giant blocks support range rendering and do not require retaining every display row.

## Authority model

The transcript has one viewport authority: a semantic anchor plus an exact local row tape.

Global display-row estimates are metadata. They may bootstrap an explicit far seek or describe a scrollbar, but they never determine local movement and never replace a pinned semantic anchor.

### Semantic viewport

```rust
enum TranscriptViewport {
    Tail,
    Pinned(ViewportAnchor),
}

struct ViewportAnchor {
    node: StableNodeKey,
    row: RowToken,
    screen_offset: u16,
}
```

`StableNodeKey` is deterministic from source block IDs and presentation kind. It does not depend on the currently loaded record page.

`RowToken` identifies an exact row for the active layout key and retains source identity needed to resolve after reflow. Synthetic rows such as separators receive deterministic node-local tokens.

`screen_offset` preserves an anchor's screen placement during reflow, content updates, and cursor-driven reveal operations.

### Exact row tape

The row tape is the bounded materialized display buffer plus one structurally aligned identity for every row. Each identity contains the exact render-node anchor and an optional content-aware anchor:

```rust
struct TranscriptExactViewport {
    tape: ExactRowTapeHandle,
    row_offset: RowIndex,
    global_total_rows: RowIndex,
    active_record_range: Option<(usize, usize)>,
}

struct ExactRowTapeHandle {
    key: ProjectKey,
    rows: MaterializedRows,
}

struct ProjectedRowIdentity {
    exact: ProjectionAnchor,
    content: Option<ProjectionAnchor>,
}

struct VisibleProjectionState {
    row_base: RowIndex,
    total_rows: RowIndex,
    row_identities: Vec<ProjectedRowIdentity>,
}
```

The backing buffer stores the corresponding display rows. The tape contains the viewport plus bounded overscan. Local navigation reuses it directly when the requested viewport is covered. The app cannot inspect the handle's key or rows directly. `TranscriptProjection` resolves the handle only while its render-plan, renderer, presentation, row, width, viewport, history, and grouping generations still match. `row_offset` maps the local tape into sparse global coordinates, while `global_total_rows` preserves the authoritative scrollbar extent when an exact local move changes only the tape's local total. At an edge, the engine loads adjacent records, resolves the previous exact top identity in the new source sequence, hydrates and measures the crossed path, then completes the exact movement.

Changing the tape's local or sparse origin does not change the semantic viewport anchor. If any required persisted body cannot be hydrated, projection preparation returns `TranscriptProjectionHydrationError`; an incomplete plan never reaches viewport materialization. Resume renders an unavailable-preview state, while the live transcript renders a bounded unavailable state.

## Components and ownership

### `TranscriptSource`

Owns the logical record sequence:

- Stable record ordinal and block ID.
- Lightweight paged record summaries.
- Lazy body loading.
- Keyed live records that may still mutate.
- Bounded metadata-page and hydrated-body caches.

Loading or evicting a page never recreates logical identity.

### `PresentationSequence`

Maps source records to stable render nodes:

- Node identity is deterministic from source block IDs and presentation kind.
- Composition is independent of cache and page boundaries.
- Grouping has explicit boundary context rather than treating the active page as the complete transcript.
- A source mutation invalidates only affected nodes and neighboring composition boundaries.

### `LayoutCache`

Owns exact node layout for a `LayoutKey`:

```text
(width, theme generation, renderer generation, renderer cache key,
 presentation generation, node content generation)
```

It exposes:

- Exact row count when measured.
- Exact row-range rendering.
- Mapping between stable row tokens and node-local display rows.
- Bounded LRU retention.

The cache supports partial rendering for giant nodes.

### `ViewportResolver`

Consumes one intent and returns one immutable applied viewport.

Local intent resolution:

1. Start from the current semantic top anchor.
2. Ensure the required exact rows exist in the row tape.
3. Walk exactly the requested number of rows.
4. Commit the resulting semantic anchor once.
5. Return a frame whose rows, top position, hit map, cursor, and selection share one coordinate system.

Progressive projection planning exactifies only the node range selected by the current bounded plan. Every changed pass exactifies at least one previously unmeasured node, and planning has an explicit budget of one pass per indexed node plus a final convergence pass. Exact row-tape movement does not use a directional numeric clamp.

### `ExtentIndex`

Owns a stable source-space model used only for:

- Scrollbar ratio and thumb size.
- Scrollbar click and drag.
- Explicit approximate row seek.
- Choosing an initial record page for a far seek.

The index is not a second viewport and does not expose estimated rows to local navigation. It consists of fixed-size source-record chunks. Every persisted record has a compact display-extent profile at canonical content widths. A chunk stores the element-wise sum of its record profiles, its source range, and its record count. Profiles include line-aware wrapping, terminal cell width, compact presentation for structured records, and one additive source-boundary allowance. They never use raw byte count divided by the current width as the primary estimate.

At an arbitrary width, each profile uses monotone interpolation between neighboring canonical widths and bounded extrapolation outside them. Chunk prefix sums provide total extent and fraction-to-source mapping without loading record bodies. Resolving a target within one chunk reads at most that chunk's bounded record summaries.

Exact layout observations may improve an in-memory profile, but they are committed as one extent revision. A revision can update scrollbar metadata and future far seeks only. It cannot change the semantic viewport anchor, exact row tape, or the meaning of a local movement already in flight. Pointer scrollbar drags capture the total-row extent, viewport height, track geometry, and thumb grab offset at pointer-down. Every drag event uses that frozen coordinate system through pointer release, even if hydration or extent refinement changes the currently painted scrollbar metadata.

A far seek is two-phase:

1. The frozen scrollbar fraction selects an estimated source record and an estimated row within that record from the bounded extent index.
2. A bounded source region starting at the target record is loaded and rendered. The estimated in-record position resolves to an exact stable render-node row anchor.

The exact tape is then rebased so the resolved semantic anchor occupies the global row requested by the frozen gesture. Hydration therefore cannot move the thumb away from the pointer. The target is considered exact only when the tape covers the full requested viewport, not merely its top row. After this commit, normal exact navigation resumes. Refining or rebuilding the index cannot move the viewport.

### Persisted extent profiles

Extent metadata is presentation-aware but not a serialized render tree. The persistence contract is:

- Canonical width buckets are defined by the lineage store format.
- Profiles are computed while transcript records are written, when bounded searchable and display text is already available.
- Every immutable transcript root has one complete set of fixed-size chunk aggregates.
- Suffix publication copies unchanged full chunks from the prior root and recomputes only the affected boundary chunk and suffix.
- Forks reuse the same immutable root and profile rows. Rewinds select a prior root, while unreachable profiles participate in bounded reclamation.
- Resume reads compact chunk rows and the active record page. It does not deserialize or render the full transcript.
- Missing or malformed profile coverage is a canonical integrity error surfaced by storage diagnostics.
- Renderer-specific exact observations remain cache data unless they have a compatible profile version and layout fingerprint.

The profile is deliberately an estimate. Accuracy is enforced statistically against exact production rendering, not assumed from a formula. The accepted error bounds are:

- Total extent relative error at or below 10 percent for large heterogeneous fixtures.
- Prefix fraction error at or below 5 percentage points at sampled source positions.
- Scrollbar thumb position error at or below one track cell where terminal quantization permits it.

Fixtures include many short hard lines, giant unbroken lines, markdown, Unicode, thinking blocks, compact tools, and heavily skewed mixtures. If these bounds cannot be met by the compact profile, the profile gains another presentation feature or width bucket instead of allowing local navigation to consume the estimate.

### `TranscriptFrame`

The viewport resolver returns one coherent applied frame:

```rust
struct AppliedTranscriptViewport {
    materialized_rows: MaterializedRows,
    top_anchor: Option<TranscriptTraceAnchor>,
    scrollbar_total_rows: RowIndex,
    exact_visible_range: Range<RowIndex>,
    placeholder_rows_visible: bool,
    scroll_state: VerticalScroll,
    cursor_range: Option<DocRange>,
}
```

The materialized buffer and projection retain one identity per row, plus the hit-test data, highlights, and decorations associated with this frame. The window receives row materialization and scroll state from the same applied result.

### Window adapter

The generic window paints the returned local rows and reports input. `row_base` remains transport metadata for mapping the bounded buffer into document APIs; it is not transcript navigation authority.

A public numeric scroll position remains for APIs and scrollbar rendering, but local navigation accepts it only as a consistency check against the last applied exact viewport. Movement is resolved from exact row identity. Estimated numeric coordinates are used only for explicit far seeks.

## Intent semantics

### Tail

Tail is an explicit mode, not a very large row number. It resolves the exact tail viewport and stays attached as content grows unless selection or user navigation pins the viewport. End-of-buffer navigation such as Vim `G` enters this mode after projection instead of seeking to an estimated numeric row.

### Local row and page movement

Wheel, keyboard row movement, page movement, and half-page movement use exact row-tape navigation. Page movement is exactly the applicable viewport row count.

### Reflow

A width or layout-key change resolves the current semantic row token under the new layout and preserves its screen offset. Numeric rows from the old layout are discarded.

### Search, reveal, and navigation

Search and navigation return source or stable-node anchors. The source loads the target region, presentation resolves the node, and the viewport pins the exact result.

### Scrollbar and approximate seek

Only these intents may use the extent index. Placeholder rows may be shown during an unresolved seek, but the committed pinned viewport always has an exact semantic anchor.

## Selection, cursor, and copy

Cursor and selection endpoints are semantic transcript positions rather than estimated global rows. Each visible display row carries a stable row key and source mapping.

- Mouse hit testing maps frame-local coordinates directly to semantic positions.
- Selection remains stable while pages load or estimates change.
- Copy walks source and presentation ranges using semantic endpoints.
- Search highlighting is attached to frame rows after semantic result resolution.

## Cache invalidation

Invalidation is keyed and local:

- Width or renderer change invalidates matching layout entries and row tapes.
- Presentation configuration change invalidates affected node composition and layouts.
- A live block mutation invalidates its node and dependent grouping boundaries.
- Loading another record page invalidates the source-sequence render plan and height index, while retaining keyed display layouts for unchanged nodes.
- Extent refinement invalidates scrollbar metadata only.

The active record range never replaces the viewport engine or discards reusable node layouts.

## Failure containment

The implementation asserts these invariants in debug and test builds:

- Every frame row has a row key.
- The viewport top resolves to a row in the returned frame.
- Local movement preserves expected overlapping row keys.
- Frame-local hit metadata and display rows have equal lengths.
- Paging and extent updates preserve a pinned top row key unless the intent moved it.
- Estimated rows are never consumed by the local movement path.
- Cache budgets remain enforced after every projection.

## Implementation sequence

This is a direct replacement, not a compatibility layer.

1. Add visual-continuity E2E tests for cold and saved/resumed sessions, including the first wheel input and tiny transient viewports.
2. Add quantitative heterogeneous extent tests that compare sparse resume metadata with exact full-render truth.
3. Establish stable presentation row identity and the exact row tape.
4. Route local and page intents through exact semantic movement.
5. Make record-page extension preserve presentation and layout caches.
6. Persist versioned per-record width profiles and transactionally maintained fixed-size chunk aggregates.
7. Replace mixed sparse totals and prefix scans with one chunk-prefix extent revision for scrollbar geometry and far seeks.
8. Separate extent metadata from applied viewport coordinates. Exact refinement preserves the semantic anchor and active gesture geometry.
9. Move resize, search, reveal, cursor, selection, and copy to semantic anchors.
10. Make the window adapter consume one coherent frame.
11. Delete byte-per-width totals, iterative local-delta rebasing, active-projection recreation, and parallel viewport fields that can drift apart.
12. Validate functional, accuracy, performance, and memory gates.

## Validation gates

### Functional

- Exact cold-transcript wheel continuity.
- Exact save/resume wheel continuity from the first input, including transient zero-height layout frames.
- Hundreds of consecutive wheel frames across record pages.
- Bidirectional symmetry.
- Tail detach and repin.
- Resize anchor preservation.
- Search and reveal across unloaded pages.
- Cursor, mouse selection, copy, folds, and grouped tools.
- UTF-8 and wide-character coverage.
- Heterogeneous total extent and prefix-fraction accuracy at every canonical width and interpolated widths.
- Scrollbar click and drag source-position accuracy before and after local hydration.
- Sparse profile refinement and root-based rewind behavior.

### Engineering

```bash
cargo nextest run --workspace --features smelt-tui/harness
cargo fmt -- --check
cargo clippy --workspace --all-targets --features smelt-tui/harness -- -D warnings
cargo llvm-cov nextest --workspace --features smelt-tui/harness --fail-under-lines 80
```

Run storybook snapshots when transcript permission or transcript rendering output changes.

### Performance and memory

Use the existing resumed transcript benchmark, including 50 MB resume and resumed wheel workloads. Require:

- Bounded hydrated, record-page, layout, row-tape, and extent-index memory.
- No store reads or work proportional to total transcript length during local scrolling.
- No full transcript body load or rendering during resume.
- Extent metadata loaded at resume is proportional to fixed-size chunks, not transcript text bytes.
- Suffix persistence rebuilds only chunks intersecting the replaced suffix.
- No regression in resumed-wheel latency or resume throughput.

## Removed concepts

The final implementation has no need for:

- Global estimated row as pinned viewport authority.
- A capped local-delta exactification and rebase loop.
- Directional `min` or `max` corrections that only enforce numeric monotonicity.
- Recreating the projection when the active sparse record range changes.
- Treating materialized row origin and projected global scroll as independently authoritative inputs.
- Sparse placeholder coordinates in ordinary local navigation.
