# Transcript Abstraction Consolidation Plan

## Goal

Consolidate the transcript rendering stack without losing behavior or performance, while extracting the generic virtualization substrate that should also work for future large buffers/files. The first concrete target is virtualized transcripts: long sessions should render only the rows needed for the current viewport and bounded range APIs.

The broad direction is “generic materialized windows, source-specific materializers”. `Window` owns viewport, scroll state, row-base math, cursor/selection rendering, and scrollbar totals. A content source such as transcript, picker, or a future large-file buffer owns how to produce a materialized `Buffer` slice for a requested absolute row range. The old `Document` abstraction was unused and incomplete, so it has been removed rather than extended.

## Implementation Status

Completed on `transcript-abstractions-plan`:

- Removed the unused `Document` abstraction and moved the surviving shared row primitives into `crates/edit/src/row.rs`.
- Added generic materialized-window primitives: `MaterializedRows`, `MaterializeRequest`, `Window::apply_materialized_rows`, and the pre-paint `Ui::render_with_paints_prepared` seam.
- Replaced transcript-specific projection output wrappers with the generic materialized-row metadata.
- Renamed transcript-private document terminology to `BlockRowIndex` / `BlockRow`.
- Made transcript range APIs range-native: bounded row requests no longer full-materialize the transcript, and range-local soft/hard break offsets are preserved.
- Switched normal transcript frames to visible projection by default.
- Kept mouse selection/copy buffer-local; while a transcript mouse drag is captured, projection is frozen so streaming appends do not invalidate the selected buffer slice.
- Kept full-transcript compatibility APIs explicit; full buffer projection is test-only and not part of the normal production frame path.
- Deferred Lua per-block/window rendering while preserving the materialization seams it will need later.

## Code Audit Summary

Important existing seams to build on:

- `Document`, `DocPos`, `ViewAnchor`, `BufferDocument`, `DisplayRow`, and `Window::render_document` were unused in production and have been removed. The active performance path is materialized buffers feeding `Window::render`, not a parallel document renderer.
- `RowIndex` and `row_to_usize` are real shared row primitives and now live in `crates/edit/src/row.rs`. They are re-exported as `smelt_edit::RowIndex` and `smelt_edit::row_to_usize` without a document namespace.
- `Window::render` remains the single production window renderer. The removed document renderer did not cover gutters, highlights, virtual text, selections, horizontal scroll, wrapping parity, cursor handling, or copy/selection behavior.
- `Window` already has the important virtualization state: `row_base`, `total_rows_override`, `set_materialized_rows`, `local_row`, `absolute_row`, and scroll totals. `scroll_top` is absolute; cursor rows and byte offsets are local to the materialized backing buffer.
- `materialized_row_range` already exists and picker already uses it with `Window::set_materialized_rows`. That is the generic fixed-height virtualization precedent.
- `UiHost` exposes text-only compatibility APIs: `rows_for_range`, `breaks_for_range`, and `visible_range`. The default range APIs intentionally fall back to full materialization, while transcript overrides now use bounded range materialization and preserve soft-wrap information for range breaks.
- `BufferParser` is a full-source render hook: it rebuilds a buffer's `lines` from `source` for a width. It is not the right virtualization seam for huge files or transcripts. In a virtualized window, the backing `Buffer` should be treated as a materialized row cache, not necessarily as the full source of truth.
- `Ui::render_with_paints_prepared` resolves split/overlay geometry, exposes a pre-paint `MaterializeRequest`, then continues through `Buffer::ensure_rendered_at`, `Window::ensure_layout`, and `Window::render`. Transcript currently uses bespoke sync before render; the generic seam is in place for other virtualized windows.
- The normal transcript frame path now uses visible projection: tail-follow requests `ScrollTarget::visible_tail()`, and pinned scrolling requests `ScrollTarget::visible_row(...)`.
- Visible transcript projection reuses a previously materialized slice when the requested viewport stays inside it; it is now the default frame path.
- Mouse selection for transcript intentionally uses the currently projected buffer and projected-buffer breaks, not full transcript breaks. This avoids stale byte offsets and must not be simplified away.
- `BufferCopy`, `Buffer::copy_range`, and `Buffer::extract_text` already exist. Transcript copy is not the only copy seam; it is a richer copier layered on generic buffer metadata.
- Tool blocks already use the safe Lua pre-pass pattern: run Lua on the main thread, convert to owned render data, then allow worker layout to consume only owned data.

## Principles

- Keep transcript block semantics in transcript code.
- Put the generic abstraction at the materialized-window boundary: absolute row range in, materialized `Buffer` slice plus `MaterializedRows` out.
- A virtualized backing `Buffer` is a cache of currently materialized rows. Do not require it to contain the whole source.
- Preserve virtualization performance everywhere: normal frames, range APIs, Lua-visible rows, block snapshots, and future block renderers should all go through the same planned/cached materialization path.
- Move only demonstrably generic behavior into `smelt_edit`/`smelt_buffer`: row primitives, materialized-row math, window coordinate conversion, and safe row metadata copying.
- Keep variable-height indexing source-specific until a second production surface needs it. Transcript can own block-height indexing; a future large-file buffer may be fixed-height and should not pay for transcript's block model.
- Do not call Lua from worker threads. Lua block/window rendering is deferred, but future cache/invalidation requirements should influence the materialization API shape.
- Preserve selection, Vim, mouse, copy, scrollbar, tail-follow, resize anchoring, tool rendering, and Lua compatibility after switching the normal transcript frame to visible-only projection.
- Treat byte offsets in a visible projection as local to the materialized slice. Never mix those offsets with full-transcript row/break data.

## Phase 0 — Freeze behavior and remove the unused document experiment

Status: complete. `RowIndex` and `row_to_usize` now live in `crates/edit/src/row.rs`; `Document`, `DocPos`, `ViewAnchor`, `BufferDocument`, `DisplayRow`, `Window::render_document`, and `crates/edit/src/document.rs` have been removed.

### 0.1 Add focused regression tests before refactoring

Add the smallest test set that makes projection changes safe:

- full vs visible projection equivalence for rows in the viewport
- soft-wrap and hard-break classification
- table/code/markdown copy semantics, including `source_text` and `copy_continuation`
- mouse selection with nonzero `row_base`
- tail-follow during streaming append
- resize anchor across width changes

These tests are not optional ceremony; they are what allows the duplicate projection paths to be deleted confidently.

### 0.2 Remove the current `Document` abstraction

The current `Document` code appears unused in production and incomplete compared with `Window::render`. It was introduced for the right performance reason—avoid full rendering of huge resumed sessions—but it is not the active mechanism delivering that performance.

Action:

- move `RowIndex` and `row_to_usize` out of `crates/edit/src/document.rs` into `crates/edit/src/row.rs`; add `pub mod row;` and top-level re-exports for both helpers
- delete the `Document` trait
- delete `DocPos`, `ViewAnchor`, `BufferDocument`, and `DisplayRow`
- remove their exports from `crates/edit/src/lib.rs`
- update call sites using `document::row_to_usize` / `smelt_edit::document::row_to_usize` to the new helper location
- remove `Window::render_document` and its unit test
- delete `crates/edit/src/document.rs` once the surviving row helpers are moved

Do not sacrifice performance when removing it. The replacement performance path is not “render less through `Document`”; it is “render less through source-specific materializers feeding generic materialized windows”.

## Phase 1 — Generic materialized-window substrate

Status: complete. `MaterializedRows` and `MaterializeRequest` live in `smelt_edit::row`, are re-exported at the crate root, and are applied through `Window::apply_materialized_rows`. `Ui::render_with_paints_prepared` provides the generic pre-paint materialization callback.

### 1.1 Keep row primitives and materialized-row helpers in one generic module

Use `crates/edit/src/row.rs` as the home for generic row primitives and row-window metadata:

- `RowIndex`
- `row_to_usize`
- `MaterializedRows`

Re-export these from `smelt_edit` at the crate root. External callers should use `smelt_edit::RowIndex`, `smelt_edit::row_to_usize`, and `smelt_edit::MaterializedRows`; they should not depend on `document::...`, and they should not need `window` internals.

Keep the initial `RowIndex` / `row_to_usize` move mechanical and separate from behavior changes.

### 1.2 Introduce one generic materialization result type

Replace transcript-specific projection output structs with a generic materialized-window result.

Expected shape:

```rust
pub struct MaterializedRows {
    pub clamped_scroll: RowIndex,
    pub row_base: RowIndex,
    pub total_rows: RowIndex,
    pub materialized_rows: RowIndex,
}

impl MaterializedRows {
    pub fn materialized_range(&self) -> Range<RowIndex>;
    pub fn contains_abs_row(&self, row: RowIndex) -> bool;
    pub fn local_row(&self, abs: RowIndex) -> RowIndex;
    pub fn absolute_row(&self, local: RowIndex) -> RowIndex;
}
```

This type should live with the generic window/row virtualization primitives, not in transcript code. `Window::set_materialized_rows` can either accept it directly or have a companion `Window::apply_materialized_rows`.

Regression checks:

- normal transcript sync still sets identical `row_base`, materialized row count, `total_rows`, and `clamped_scroll`
- resume preview rendering still sets identical materialized rows
- picker virtualization still computes the same range and scrollbar totals

### 1.3 Add a generic pre-paint materialization seam

The missing generic piece is not a new renderer. It is a hook after layout geometry is known and before `Window::render` where a source can update a backing `Buffer` with the rows needed for that window's current absolute viewport. For virtualized windows this hook must run before `Buffer::ensure_rendered_at` / `Window::ensure_layout` operate on the materialized backing buffer. Non-virtual buffers keep the existing path unchanged.

Recommended concrete request shape:

```rust
pub struct MaterializeRequest {
    pub win: WinId,
    pub buf: BufId,
    pub rect: Rect,
    pub content_width: u16,
    pub scroll_top: RowIndex,
    pub follow_tail: bool,
}
```

The source-specific materializer should:

1. choose a bounded absolute row range, usually with `materialized_row_range`
2. write only that range into the backing `Buffer`, preserving row metadata
3. return `MaterializedRows`
4. let the caller apply it to `Window`

Do not require a broad public trait immediately. A concrete request/output pair used by transcript and picker is enough. Add a trait only when a second independently-owned source needs registration.

Implementation gap to close: `Ui::render_with_paints` currently resolves split/overlay geometry internally. To support arbitrary virtualized windows, add a preparation callback or split the render pass so the host can receive `(WinId, BufId, Rect, content_width, gutter_width, scroll_top, follow_tail)` after layout resolution and before `buf.ensure_rendered_at(content_width)`. The callback materializes registered virtual windows, then the existing `ensure_rendered_at` / `ensure_layout` / `Window::render` flow continues unchanged.

Important: do not use `BufferParser` as this seam. `BufferParser` renders a full `source` string into full `lines`; virtualized sources need to write a materialized slice without storing or parsing the whole document in the backing buffer.

### 1.4 Keep `Window::render` as the only production renderer

After Phase 0, `Window::render_document` should be gone. Do not rebuild an equivalent document-render path. The materialization seam must feed normal `Buffer` data into `Window::render`, so gutters, highlights, virtual text, selections, cursor rendering, horizontal scroll, and scrollbars stay on one rendering path.

A future document-driven rewrite is acceptable only if it deletes the display-buffer projection layer and makes all window rendering use one renderer. That is out of scope for this plan.

## Phase 2 — Transcript as the first robust virtual source

Status: complete for the consolidation scope. Transcript projection now returns generic `MaterializedRows`, uses private `BlockRowIndex` terminology, and has range-native `rows_for_range` / break materialization that preserves soft and hard breaks relative to the returned range text.

### 2.1 Collapse duplicate projection output types

`TranscriptData` in `crates/tui/src/app/transcript.rs` and `ProjectOutput` in `crates/tui/src/content/transcript_buf.rs` carry the same row-window fields. Remove the wrapper and return/apply the generic `MaterializedRows` type from Phase 1. Map transcript's current `projected_rows` field to generic `materialized_rows` at the boundary; do not keep transcript terminology in the generic type.

### 2.2 Rename transcript's height index to avoid `Document` terminology

`TranscriptDocument` is not the removed `Document` abstraction; it is a private variable-height row index. Rename it to something like `TranscriptRowIndex` or `BlockRowIndex` while consolidating projection. This removes ambiguity and makes the abstraction boundary clearer.

Keep it private until another production surface needs variable-height item indexing.

### 2.3 Extract one transcript block materialization loop

The same block/gap/height/cache metadata logic appears in full projection, visible projection, full row materialization, line-break materialization, and block-layout snapshots. Extract a small private helper that iterates rendered blocks and centralizes:

- resolving `BlockId` + `LayoutKey`
- reading `BlockBufferCache`
- applying `rendered_block_gap`
- updating exact heights in the transcript row index
- emitting gap rows as hard breaks
- copying row text, highlights, virtual text, and `LineDecoration`
- reporting absolute block start rows

Do not make this a public abstraction yet. The immediate goal is to make full and visible projection use the same row metadata and height math.

Regression checks:

- full projection output is byte-for-byte unchanged for representative transcript fixtures
- visible projection output matches full projection for rows in the viewport
- `visible_block_layout()` reports the same block starts as before
- `line_breaks()` keeps soft breaks based on the next row's `LineDecoration::soft_wrapped`
- streaming incremental projection is unchanged for the full projection path

### 2.4 Add a narrow buffer row-copy helper only if it removes manual metadata copying

Transcript projection manually copies lines and row metadata from per-block buffers into the display buffer. If the extracted loop still has repeated metadata plumbing, add a narrow `Buffer` helper for copying rendered rows between buffers.

Possible shape:

```rust
impl Buffer {
    pub fn append_rendered_rows_from(&mut self, src: &Buffer, range: Range<usize>);
}
```

This helper must preserve:

- row text
- highlights and `SpanMeta`
- virtual text
- `LineDecoration` fields: `soft_wrapped`, `cell_selectable`, `block_selectable`, `copy_continuation`, `source_text`, `source_line`, `pre_formatted`, `fill_bg`

It must not copy source text, parser state, undo state, selection, buffer ids, or attachment ids unless explicitly designed to do so.

### 2.5 Make transcript range APIs use the same materialization pipeline

`UiHost::rows_for_range` and `breaks_for_range` are text compatibility seams for Lua, search, and host-level operations. The transcript override should stop materializing the full transcript for bounded ranges.

Implementation direction:

- add a `TranscriptProjection::rows_for_range(...)` helper that uses the same planning/materialization path as frame rendering
- prefer one range materialization pass that can return rows plus soft/hard breaks, so `rows_for_range` and `breaks_for_range` do not independently rebuild the same slice
- expand to whole blocks internally, but return only rows in the requested range
- add `TranscriptProjection::breaks_for_range(...)` that preserves both soft and hard breaks relative to the returned `rows.join("\n")` string
- keep `rows_for` and `breaks_for` as explicit full compatibility APIs

Important edge case: range-local byte positions are not full-transcript byte positions. State that `breaks_for_range` returns offsets relative to the joined range text, matching the default implementation.

## Phase 3 — Copy and selection consolidation without breaking transcript semantics

Status: partially complete by preservation rather than rewrite. Transcript copy policy remains in `TranscriptCopier`, and mouse selection remains tied to the currently materialized buffer. The visible-projection switch added one important guard: during an active transcript mouse drag, projection is frozen so streaming appends cannot rematerialize the backing buffer and invalidate buffer-local selection offsets.

### 3.1 Do not replace `TranscriptCopier` with `Buffer::extract_text` in one step

`Buffer::extract_text` already handles unselectable spans and `copy_as`, but transcript copy additionally:

- keeps kill-ring raw source separate from clipboard display text
- prefers `LineDecoration::source_text` for fully covered rows
- skips duplicate `source_text` on `copy_continuation` rows
- merges soft-wrapped rows without inserting newlines
- treats `copy_continuation` independently from soft wrap

Refactor toward shared helpers only after tests pin these behaviors. A likely safe first step is to move reusable row-cell emission and selectable-span logic from transcript copy into buffer code, leaving `TranscriptCopier` as the policy owner.

Regression checks:

- Markdown table copy preserves source text.
- Code block copy preserves raw text in the kill ring and user-facing text in the clipboard.
- Non-selectable chrome is dropped.
- `copy_as` spans emit exactly once per span.
- Soft wraps merge without newlines; hard breaks insert newlines.
- `copy_continuation` rows coalesce even when they are hard selection boundaries.

### 3.2 Keep mouse selection tied to the materialized buffer

Transcript mouse selection currently builds breaks from the projected display buffer immediately before calling `Window::handle_mouse`. This is intentional. When visible projection becomes the default, this remains the correct source of truth because selection byte offsets are local to the materialized buffer.

Do not change mouse selection to use full transcript rows or full transcript breaks. Selection byte offsets are projected-buffer coordinates. If a future rewrite introduces document-native selection, convert mouse hits to stable transcript positions first and make copy/range operations document-native in the same rewrite.

Edge cases to test:

- double-click word selection over a soft-wrapped row
- triple-click block selection within a visible slice whose `row_base` is nonzero
- drag selection while autoscrolling causes the viewport to rematerialize
- click snapping near non-selectable table borders
- selection/yank flash subtracts `row_base` before writing buffer-local selection rows

### 3.3 Separate full-transcript operations from frame projection

Some operations may currently get full-transcript behavior only because the normal frame materializes the full transcript buffer. Do not make frame rendering full again for those operations. Audit the call sites and route any required full/range behavior through explicit transcript helpers.

Callers to audit before visible projection becomes default:

- Vim motions and text objects that currently operate on the backing buffer
- copy/yank ranges that might extend outside the visible slice
- Lua `smelt.transcript.blocks()` compatibility
- exact `block_at_row(row)`
- any future transcript text search API that needs whole-transcript semantics

Make the call site explicit: visible-frame projection is for painting and local mouse interaction; full/range APIs are for transcript-wide queries.

## Phase 4 — Enable visible transcript projection by default

Status: complete for normal frames. The render loop now requests visible transcript projection for both tail-follow and pinned-scroll frames. Full-transcript compatibility APIs remain explicit, and full buffer projection is test-only.

The normal render loop now uses visible projection after Phases 1-3 established the generic materialization substrate, range-native transcript APIs, and buffer-local selection/copy behavior.

Conceptual render-loop change:

```rust
let transcript_scroll_target = if self.ui.should_follow_tail(TRANSCRIPT_WIN) {
    ScrollTarget::visible_tail()
} else {
    ScrollTarget::visible_row(self.transcript_win().scroll_top())
};
```

Implemented preconditions and remaining considerations:

- `UiHost::rows_for_range` and `breaks_for_range` are range-native and preserve soft breaks.
- Full-transcript operations have explicit full/range paths.
- Mouse selection remains local to the materialized buffer and has tests for nonzero `row_base`.
- Cursor restoration understands that `cursor_row` is local while `scroll_top` is absolute.
- Projection cache invalidation has a clear generation hook for future renderer/theme/reload changes, so visible projection reuse cannot accidentally reuse stale rows later.
- Visible projection has enough overscan or block pre-roll for upward scrolling near the start of the current materialized slice.
- Streaming append performance is acceptable. The existing incremental fast path applies to full projection; visible-tail projection may need its own tail fast path or measured proof that it is unnecessary.

Regression checks:

- tail-follow while streaming appends
- user scroll pinned away from tail during append
- wheel coalescing and scrollbar dragging
- resize preserves visible block/offset
- cursor screen-row preservation across width changes
- Vim visual selection across projection rebuilds
- mouse drag selection and autoscroll
- copy/yank across materialized range boundaries
- search and text object behavior
- `smelt.transcript.blocks()` remains exact/full-compatible
- `smelt.transcript.visible_blocks()` reflects the current visible projection
- prompt/layout width changes invalidate block layout caches correctly

## Deferred direction — Lua per-block rendering

Lua per-block/window rendering is a future feature, not part of this consolidation implementation. The consolidation should still leave the right seams so Lua can later provide owned materialized rows or per-block render data without another projection rewrite.

Future constraints:

1. Projection planning determines the block ids that may be materialized.
2. Main thread invokes Lua render callbacks for cache misses only.
3. Lua output is converted into owned render data.
4. Worker block layout consumes only owned render data.
5. Cache keys include block layout key plus renderer/theme/reload generation.

Never call Lua from `BlockBufferCache::ensure_many` workers.

Likely API shape, deferred:

```lua
smelt.transcript.render("assistant", function(block, ctx)
  return smelt.layout.markdown { source = block.content, width = ctx.width }
end)
```

Initial non-goals:

- no direct mutation of the transcript display buffer by block renderers
- no migration of current tool block rendering until the generic API is proven
- no Lua renderer implementation before visible projection and range-native APIs are stable

## Phase 5 — Optional larger simplifications

Only after visible projection is stable:

- Reconsider whether `TranscriptView` should remain a façade or merge into a clearer `TranscriptState`.
- Reconsider `ScrollTarget`: separate materialization mode from scroll anchor and derive the anchor from `Window` scroll state where possible.
- Consider making transcript block snapshots use the shared projection/range pipeline directly instead of materializing all blocks unless exact rows are requested.
- Consider a document-driven rendering rewrite only if it deletes the display-buffer projection layer and makes all window rendering use one renderer. Do not keep both systems.

## Regression Test Matrix

Add or update tests around:

- generic `MaterializedRows` local/absolute row helpers clamp/saturate correctly
- generic pre-paint materialization leaves non-virtual buffers unchanged
- a virtualized fixed-height source can materialize a small range without storing all rows in the backing buffer
- full vs visible transcript projection equivalence for rows in the viewport
- `row_base`, `materialized_rows`, `total_rows`, and scrollbar totals
- tail-follow during streaming append
- scroll pinned away from tail during append
- resize anchor across width changes
- cursor screen-row preservation across projection rebuilds
- selection across soft wraps and hard breaks
- triple-click block selection with nonzero `row_base`
- copy of markdown, code, table, user, tool, exec, and compacted rows
- `UiHost::rows_for_range` does not full-materialize transcript for small ranges
- `UiHost::breaks_for_range` preserves soft-wrap breaks
- picker virtualization still works
- future Lua tool/render pre-pass remains main-thread-only once implemented
- future renderer cache invalidates on width, theme, reload, renderer change, and block mutation
- visible projection reuse is invalidated by generation/theme/future renderer changes

## Resolved Decisions

1. Current `Document` abstraction should be removed, not extended.
   - Move `RowIndex` and `row_to_usize` first because they are real shared row primitives.
   - Delete the unused trait/types/render path after that move.

2. The durable abstraction is materialized windows, not transcript-only projection or document rendering.
   - `Window` owns row-base/scroll/render behavior.
   - Content sources own materializing a bounded row range into a backing `Buffer`.
   - `BufferParser` remains a full-buffer parser, not the virtualization hook.

3. Full-transcript behavior must be explicit after visible projection.
   - Frame rendering and visible plugins should use visible/range APIs.
   - Full compatibility APIs such as `blocks()` and internal full projection remain explicit paths rather than side effects of normal frame rendering.

4. `smelt.transcript.blocks()` remains exact and full-materializing for compatibility.
   - `visible_blocks()` remains the frame-local API.

5. Lua per-block rendering is deferred.
   - Use the existing tool path as the safety pattern later.
   - Do not migrate tool block rendering until the generic API removes code.

6. Lua renderers should not mutate transcript buffers directly.
   - Declarative output is safer for caching, threading, and invalidation.

7. Transcript mouse selection remains buffer-local under visible projection.
   - During active transcript drags, projection is frozen to preserve the clicked slice while streaming appends continue.
