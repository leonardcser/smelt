# Selection architecture rewrite plan

## Goal

Make transcript/viewer virtualization and selection simpler, more unified, and less error-prone. The user-visible invariants are:

> Regular render/scroll/cursor movement materializes only the visible transcript window plus bounded overscan.
>
> There is one normalized selection range. Render, copy, yank flash, keyboard visual mode, mouse drag, and autoscroll all consume that same range/head.

If two paths re-derive the range independently, we should treat that as a design smell.

This plan is a living design document, not a final specification. If implementation exposes a simpler or more correct design that satisfies the same invariants with less code and no intermediate scaffolding, take that design and update this document to match the new understanding. Re-evaluate the plan at phase boundaries and whenever a proposed abstraction starts adding complexity instead of deleting it. Do not preserve plan steps for their own sake. Prefer larger logical commits that leave the product in a coherent state over many small scaffolding commits.

## Relevant implementation files

Primary plan and staged work:

- `selection-architecture-rewrite-plan.md` — this architecture plan.
- `crates/buffer/src/coords.rs` — byte/display row-range projection and selectable-row helpers.
- `crates/edit/src/lib.rs` — `UiHost` trait, `rows_for_range`, `copy_virtual_range`, and future search/range seams.
- `crates/edit/src/window.rs` — common `Window` shell, viewport math, cursor hit-testing, selection projection, `snap_col_past_chrome`.
- `crates/edit/src/window/virtual_rows.rs` — rows-mode cursor, visual selection, virtual commands, and mouse hit-testing.
- `crates/edit/src/vim.rs` — Vim command handling for `gg`, `G`, visual mode, `/`, `?`, `n`, and `N` integration.
- `crates/tui/src/app/events.rs` — focus-aware key routing, overlay/dialog dispatch, and generic viewer command dispatch.
- `crates/tui/src/app/mouse.rs` — transcript/content mouse dispatch and the transcript-specific snapping to remove.
- `crates/tui/src/app/render_loop.rs` — normal render path; should materialize only visible rows plus bounded overscan.
- `crates/tui/src/app/transcript.rs` — transcript view wrappers, block snapshots, visible rows, and selection highlight helpers.
- `crates/tui/src/app/ui_host.rs` — TUI-side `UiHost`; currently contains full-materialization hotspots such as `virtual_total_rows`.
- `crates/tui/src/app/cmdline.rs` — existing `:` bottom status input to generalize into command/search modes.
- `crates/tui/src/app/cmdline_edit.rs` — reusable editing primitives for the status input.
- `crates/tui/src/app/content_keys.rs` — content-pane wrapper around generic viewer key dispatch.
- `crates/tui/src/app/test_harness.rs` — TUI regression tests for Vim/selection/search behavior.
- `runtime/lua/smelt/prompt_bar.lua` — prompt top/bottom bar windows, focus/selectability config, and current press handlers.
- `runtime/lua/smelt/_bar.lua` — prompt/status bar composition and per-span `selectable = false` metadata for dash/fill chrome.
- `crates/tui/tests/**` — integration/storybook coverage if a behavior is better asserted by rendered frames.
- `crates/tui/src/content/transcript_buf.rs` — `TranscriptProjection`, `BlockRowIndex`, full-materialization hotspots, copy/range/search row extraction.
- `crates/core/src/content/highlight/syntax.rs` — code-block rendering and trailing-padding bug source.
- `crates/core/src/content/builder.rs` — `LineBuilder`, `SpanMeta`, `fill_line_bg`, and row background/padding primitives.
- `crates/core/src/content/mod.rs` — box/right-border helpers that may require non-selectable real pad cells.

Reference implementation for comparison, not a template to copy wholesale:

- `/Users/leo/dev/thirdparty/tmux/window-copy.c` — copy-mode viewport-bounded redraw, backing coordinates, selection scan.
- `/Users/leo/dev/thirdparty/tmux/tmux.h` — `grid`, `screen`, and copy-mode data structures.
- `/Users/leo/dev/thirdparty/tmux/grid.c` — grid/history storage behavior.

## Code findings after investigation

### 1. Keep `Window` as the common shell; move interaction state into `WindowSurface`

`Window` currently mixes common window state with text-coordinate state:

- common/layout/viewport/scroll fields: `crates/edit/src/window.rs:264`
- local byte cursor/selection fields: `cpos`, `selection_anchor`, `vim_state`, `drag_endpoint`
- virtual row state: `virtual_rows: Option<VirtualRowsState>`

A top-level `enum WindowModel { Buffer, Viewer }` is too blunt: common layout/viewport/render plumbing is genuinely shared. The better split is a stable `Window` shell plus `surface: WindowSurface`, where the surface owns both interaction role and document state.

Target shape:

```rust
struct Window {
    id: WinId,
    buf: BufId,
    config: SplitConfig,
    viewport: Option<WindowViewport>,
    scroll: ScrollState,
    layout: WrappedLayout,
    surface: WindowSurface,
}

enum WindowSurface {
    EditableText(BufferDocState),
    ReadonlyText(TextDocState),
    SelectableText(TextDocState),
    List(ListDocState),
    Inert,
}
```

`WindowSurface` should replace the old boolean authority and the bolted-on `virtual_rows` mode, not sit beside them as a compatibility label.

Implementation note: the first Phase 1 surface step replaced the direct
`focusable`, `selectable`, and `mouse_scroll` booleans with a `WindowSurface`
interaction role (`EditableText`, `ReadonlyText`, `SelectableText`, `List`,
`Inert`). This is intentionally not the full end state yet: text/list document
state still lives in the existing `Window` fields, and `virtual_rows` is still
separate. The important line is that focus, generic text selection, caret commit,
and wheel-scroll policy now ask the surface instead of reinterpreting three
booleans at each call site.

### 2. Materialized row space and interaction mode should be separate concepts

Virtual rows are not only transcript. They are also used by:

- transcript render projection: `crates/tui/src/app/render_loop.rs:168`
- resume preview/session projection: `crates/tui/src/lua/api/session.rs:600`
- picker virtualization: `crates/tui/src/picker.rs:289`

Picker rows are virtualized, but its selected item is primarily list state in `picker_state`, not a text selection. So the plan should not make “viewer text selection” synonymous with “materialized rows.”

Use explicit state per surface/document type:

```rust
enum TextDocState {
    Buffer(BufferTextState),
    Rows(RowTextState),
}

struct BufferTextState {
    cursor: usize,
    preferred_cell_col: Option<usize>,
    selection: SelectionState<usize>,
    drag: DragState<usize>,
    yank_flash: Option<TimedRange<TextRange>>,
}

struct RowTextState {
    materialized: MaterializedRows,
    cursor: DocPosition,
    preferred_cell_col: Option<usize>,
    selection: SelectionState<DocPosition>,
    drag: DragState<DocPosition>,
    yank_flash: Option<TimedRange<TextRange>>,
}

struct ListDocState {
    selected: usize,
    scroll: RowIndex,
}
```

End state: local `cpos` belongs only to byte-backed text state or becomes a private projection cache for row-backed text; it is not an alternate cursor authority.

### 3. TUI transcript mouse snapping duplicates edit hit-testing

Transcript mouse handling snaps in TUI before calling edit:

- `transcript_mouse_cell`: `crates/tui/src/app/mouse.rs:78`
- `snap_event_for_selection`: `crates/tui/src/app/mouse.rs:488`
- virtual edit hit-test then recomputes the position: `crates/edit/src/window/virtual_rows.rs:463`

These two paths can disagree about viewport-edge row clamping, materialized row base, horizontal scroll, and stale viewport scroll. This is likely the source of “sometimes offset.”

Plan change: remove transcript-specific pre-snapping from TUI. TUI should pass raw `MouseEvent`, `WindowViewport`, and click count. `Window` should perform one visible-row hit-test using the backing buffer's spans.

Implementation note: the TUI pre-snap path (`transcript_mouse_cell` and
`snap_event_for_selection`) has been removed. Transcript mouse handling now
passes the raw `MouseEvent` into `Window::handle_mouse` /
`Window::handle_virtual_mouse`; edit-side hit-testing owns row clamping,
horizontal scroll, gutter/padding subtraction, and selectable-span snapping.

### 4. Existing buffer metadata is enough for generic selectable snapping

Selection/cursor snapping can be generic because rows already expose:

- highlight spans with `SpanMeta { selectable, copy_as }`
- row decorations such as `source_text`, `soft_wrapped`, `copy_continuation`, `pre_formatted`
- `Buffer::byte_at_display_pos` and `Buffer::display_cursor_pos`, which already handle projection maps

`Window::cpos_at_visual` already calls `snap_col_past_chrome`: `crates/edit/src/window.rs:844`. This is the right location to generalize into a real hit-test that returns before/after positions rather than just one caret byte.

### 5. The full-width user chrome bug is real and has an existing primitive for the root fix

User/exec chrome blank rows currently write full-width non-selectable spaces:

- `crates/tui/src/content/transcript_parsers/chrome.rs:32`
- `LineBuilder::pad_row_to_layout_width`: `crates/core/src/content/builder.rs:199`

`LineBuilder` already has the intended alternative:

- `fill_line_bg`: `crates/core/src/content/builder.rs:214`

So blank chrome rows should probably become semantic empty rows with `fill_line_bg(SmeltUserBg)` instead of full-width non-selectable text. This avoids needing a selection special case for “all chrome reaches the viewport edge.”

Non-blank chrome rows may still use explicit left padding plus trailing background, but the blank top/bottom padding rows should not create fake text.

### 6. Copy semantics are already rightly transcript-owned

Virtual transcript copy cannot be a normal local byte copy because the buffer is only a materialized slice. The existing seam is good:

- trait method: `UiHost::copy_virtual_range` in `crates/edit/src/lib.rs:2135`
- transcript implementation: `crates/tui/src/content/transcript_buf.rs:892`

Do not remove this seam. The rewrite should instead guarantee that the `DocRange` passed to it is the same range that render/yank flash use.

### 7. `UiHost::rows_for_range` is the right seam for off-viewport text

Virtual word motions already need rows outside the materialized slice and use host callbacks:

- `resolve_virtual_viewer_command`: `crates/tui/src/app/events.rs:1159`
- `UiHost::rows_for_range`: `crates/edit/src/lib.rs:2103`

Do not force edit/window to own full transcript data. Edit should own visible hit-testing and selection state; host/projection should still provide off-viewport rows and virtual copy.

### 8. Soft-wrap/copy-continuation is mostly copy, not selection painting

`copy_continuation`, `source_text`, and `soft_wrapped` are crucial in `copy_byte_range`: `crates/tui/src/content/transcript_buf.rs:1045`.

Selection painting mostly needs rows, cells, empty-row behavior, and selectable masks. The shared projector should not overfit copy semantics. Copy/render share the same normalized range, but they may consume different row metadata.

### 9. Code-block padding is currently real selectable text

Code blocks render actual content, then pad the row to the code-block background width with normal selectable spaces:

- code-block row padding: `crates/core/src/content/highlight/syntax.rs:98`
- `LineBuilder::print` emits `SpanMeta::default()`, which is selectable: `crates/core/src/content/builder.rs:141`
- cursor hit-testing snaps only around non-selectable spans: `crates/edit/src/window.rs:844`
- current chrome snapper only sees `SpanMeta { selectable: false }`: `crates/edit/src/window.rs:2430`

So a click far to the right inside a code block can land the cursor inside padding instead of snapping to the end of the actual code text. This is a representation bug, not a cursor-mode special case.

Plan change: code-block trailing background must not be searchable/selectable/cursor-addressable text. Prefer row background fill via `LineBuilder::fill_line_bg`; if a right border or box layout requires real pad cells, emit that pad with `SpanMeta { selectable: false, copy_as: None }`. Actual code text remains selectable.

### 10. `:` cmdline already has the right status-input shape

The command line is a one-row modal overlay anchored at the screen bottom:

- opens a one-line buffer with `:` prefix: `crates/tui/src/app/cmdline.rs:43`
- applies statusline background with `fill_bg`: `crates/tui/src/app/cmdline.rs:80`
- owns text editing/history/completion while focused: `crates/tui/src/app/cmdline.rs:148`
- submits through `cmdline_submit`: `crates/tui/src/app/cmdline.rs:347`
- `:` opens it outside insert mode: `crates/tui/src/app/events.rs:390`

Search should reuse this UI pattern rather than invent a second prompt surface. Generalize it into a status input with modes, for example:

```rust
enum StatusInputMode {
    Command,
    Search { target: WinId, direction: SearchDirection },
}
```

The mode determines prefix (`:` or `/`/`?`), history bucket, completer behavior, and submit action. Editing primitives can stay shared.

### 11. Search must be generic over the focused non-editable window

Viewer key dispatch is already generic across transcript and overlay leaves:

- content pane delegates to `dispatch_window_viewer_key`: `crates/tui/src/app/content_keys.rs:24`
- overlay key cascade routes read-only viewer keys before catch-all fallbacks: `crates/tui/src/app/events.rs:985`
- `dispatch_window_viewer_key` handles transcript, overlay leaves, and future scrollable windows: `crates/tui/src/app/events.rs:1066`
- `UiHost::rows_for_range` is already the generic bounded row-access seam: `crates/edit/src/lib.rs:2101`

Search should target the currently focused non-editable window: transcript, overlay viewer, dialog viewer, or any future readonly row window. If focus is inside an editable buffer, `/` remains text input or normal Vim behavior; it must not steal focus into search.

### 12. Search needs precomputed matches for fast `n`/`N`

The desired interaction should feel nvim-like while keeping tmux's backing-coordinate discipline:

1. Press `/` while focused on a non-editable viewer.
2. The bottom status input opens with `/` and takes focus, analogous to nvim's search command-line and Smelt's existing `:` command-line.
3. Enter a query and press Enter.
4. The target window shows all matches highlighted, like persistent `hlsearch` for that viewer.
5. `n` jumps to the next match in the submitted search direction.
6. `N` jumps in the opposite direction.
7. `Esc` cancels active search and clears search highlights.

The tmux-inspired part is the coordinate model, not the UX: matches are stored in exact backing/document coordinates, visible redraw is viewport-bounded, and jumps scroll/materialize only the target row range.

That means the initial search submit should scan the target document and store match ranges. `n`/`N` should move through an already-computed `Vec<DocRange>` and scroll/materialize the target window as needed. Repeated `n` on a huge transcript should be limited by scroll/render projection, not by search scanning.

The search scan is an explicitly full-document operation, but it must not concatenate all rows. It should walk rows through the same row-provider/search-provider abstraction that virtualization uses.

### 13. Prompt bar chrome is selectable-surface chrome, not caret text

Prompt top/bottom bars are visually part of the prompt block, but they are not editable prompt input. They may contain selectable text, and the top bar currently opts into selection:

- top bar window is `focusable = false, selectable = true`: `runtime/lua/smelt/prompt_bar.lua:284`
- bottom bar window is `focusable = false, selectable = false`: `runtime/lua/smelt/prompt_bar.lua:291`
- bar dash/fill spans are emitted as `selectable = false`: `runtime/lua/smelt/_bar.lua:112`
- current prompt-bar press handlers focus the prompt on every press: `runtime/lua/smelt/prompt_bar.lua:301` and `runtime/lua/smelt/prompt_bar.lua:305`
- generic selectable leaves route clicks through `handle_selectable_leaf_mouse`: `crates/tui/src/app/mouse.rs:365`

Bug: clicking a non-selectable bar dash (`────`) can snap to the nearest selectable span, commonly the right-side status group, and move a cursor/selection head there. Since the bar chrome is non-focusable and non-selectable, a plain click on chrome should not move any caret. It should not focus the prompt, and it should not create a snapped selection endpoint.

Desired policy:

- Non-focusable selectable surfaces may support drag-copy of selectable text.
- A press on non-selectable chrome/fill is inert except for Lua pointer callbacks that explicitly want it; it must not move prompt `cpos`, bar `cpos`, app focus, or a selection head.
- Selection starts only when the initial press lands on selectable text, or when a drag crosses into selectable text according to a deliberate drag policy.
- Drag/copy from prompt-bar text should use that bar window's selection range and clipboard output, not the prompt input cursor.
- Prompt block focus promotion should distinguish the prompt input from prompt-bar chrome; “same visual block” must not mean “all chrome clicks focus the editable prompt.”

This is the same general rule as transcript/code-block chrome: non-selectable spans should not be cursor-addressable just because selectable text exists elsewhere on the row.

## Virtualization and tmux comparison

### 1. What tmux does

Tmux copy-mode is a useful comparison, but it is not a direct template for Smelt.

Tmux stores terminal contents in a cell grid:

- `struct grid` has dimensions, history size/limit, scroll generation counters, and `linedata`: `/Users/leo/dev/thirdparty/tmux/tmux.h:862`
- grid history and visible rows share one absolute coordinate space; history is `0..hsize`, visible pane rows are `hsize..hsize+sy`: `/Users/leo/dev/thirdparty/tmux/grid.c:26`
- empty lines are cheap until written: `/Users/leo/dev/thirdparty/tmux/grid.c:30`
- history collection frees old lines and shifts the remaining line metadata: `/Users/leo/dev/thirdparty/tmux/grid.c:389`
- scrolling appends one line to history and updates counters: `/Users/leo/dev/thirdparty/tmux/grid.c:437`
- reading a cell outside allocated line storage returns the default cell: `/Users/leo/dev/thirdparty/tmux/grid.c:591`

Copy-mode then keeps two screens:

- `backing`: the full grid being copied
- `screen`: the small visible copy-mode screen
- `oy`: scroll offset into the backing grid
- selection endpoints in backing/screen coordinates: `/Users/leo/dev/thirdparty/tmux/window-copy.c:256`

The important behavior is bounded redraw, not low memory use. `window_copy_write_line` copies one visible row from `backing` into `screen`: `/Users/leo/dev/thirdparty/tmux/window-copy.c:5135`. `window_copy_write_lines` loops the requested visible rows: `/Users/leo/dev/thirdparty/tmux/window-copy.c:5224`. `window_copy_redraw_screen` redraws exactly `screen_size_y(&data->screen)` rows: `/Users/leo/dev/thirdparty/tmux/window-copy.c:5293`.

Navigation also avoids rebuilding text. `window_copy_goto_line` computes `hsize` and adjusts `data->oy`: `/Users/leo/dev/thirdparty/tmux/window-copy.c:4813`. Copying a selection is allowed to scan the selected backing rows: `/Users/leo/dev/thirdparty/tmux/window-copy.c:5591`.

### 2. What Smelt should copy from tmux

Copy these ideas:

- one persistent backing coordinate space
- a small visible materialized screen/buffer
- scroll position as an offset into backing coordinates
- generation counters to know when backing/layout changed
- visible redraw bounded by viewport height
- full scans only for explicitly full-document operations

Do not copy tmux's data model wholesale. Tmux can store every terminal cell because terminal output is already a fixed-width cell stream with a configured history limit. Smelt transcript rows are derived from structured blocks, wrapping width, show-thinking state, sidecars, decorations, selectable spans, and theme-dependent highlight resolution. Storing every rendered cell forever would duplicate the block history and make width/theme changes expensive.

Recommended direction: Smelt should use a hybrid model.

### 3. Current Smelt hotspots

The current code already has the right visible-buffer direction, but several seams still force full materialization:

- render path plans visible projection in `render_loop.rs`, then applies `MaterializedRows`: `crates/tui/src/app/render_loop.rs:151`
- planning currently calls `plan_projection_measured`, which calls `measure_all_heights`: `crates/tui/src/content/transcript_buf.rs:419`
- `measure_all_heights` calls `ensure_all`, rendering every block into the cache before selecting the visible range: `crates/tui/src/content/transcript_buf.rs:381`
- `virtual_total_rows` calls `full_transcript_display_text`, which builds/clones all display rows just to return a length: `crates/tui/src/app/ui_host.rs:112`
- `rows_for_range` calls `materialize_block_layout`, which currently ensures all blocks first: `crates/tui/src/content/transcript_buf.rs:844`
- `copy_range` does the same before materializing the selected block range: `crates/tui/src/content/transcript_buf.rs:892`
- `line_breaks` materializes all blocks and then collects all rows: `crates/tui/src/content/transcript_buf.rs:939`
- `build_rows` caches a full `Arc<Vec<String>>` for legacy/full-text consumers: `crates/tui/src/content/transcript_buf.rs:796`

`BlockRowIndex` is the right seed of the tmux-like backing coordinate space: it stores block ids, layout keys, estimated/exact heights, and prefix rows: `crates/tui/src/content/transcript_buf.rs:37`. The issue is that exactness is obtained by rendering every block too often and some callers still ask for the whole row vector.

### 4. Proposed virtualization architecture

Keep `TranscriptProjection` as the transcript-owned backing projection. Split its responsibilities more clearly:

```rust
struct TranscriptProjection {
    row_index: BlockRowIndex,
    layout_cache: BlockLayoutCache,
    visible: VisibleProjection,
}
```

`BlockRowIndex` should be the durable coordinate authority:

- one node per transcript block
- node key includes block id/content generation, width, show-thinking, and any layout-affecting state
- node stores estimated height and optional exact height
- prefix rows are rebuilt from exact-or-estimated heights
- exact heights are updated whenever a block layout is rendered or measured
- top/bottom and scrollbar math read this index, not a full row vector

`layout_cache` should be bounded and keyed by rendered block identity:

- key: block id + resolved `LayoutKey` + theme/highlight generation if needed for spans/colors
- value: rendered block buffer rows, decorations, spans, source/copy metadata, and exact line count
- eviction: LRU or generational, with visible/overscan blocks pinned for the current frame
- width or show-thinking changes invalidate layout keys; theme changes invalidate color/span rendering but should not require throwing away height information unless layout actually depends on theme

`visible` should be the only thing copied into the edit `Buffer` on normal render:

- absolute `row_base`
- total rows from `BlockRowIndex`
- block layout entries for visible Lua APIs
- materialized rows covering viewport plus bounded overscan
- generation/key of the target buffer materialization

Normal scroll/render should do:

```text
scroll target -> row index block range -> ensure visible block layouts -> copy visible rows into Buffer -> apply MaterializedRows
```

It should not do:

```text
scroll target -> render every block -> concatenate every row -> slice viewport
```

### 5. Height exactness policy

User-visible row coordinates must be exact. Estimates may exist only as internal cache warmup hints; they must not drive `virtual_total_rows`, scrollbar thumb math, `gg`, `G`, search origins, search match ranges, selection ranges, or copy ranges.

Required behavior:

- `gg` jumps to absolute row `0`.
- `G` jumps to the exact bottom clamp, based on exact total rows.
- `virtual_total_rows` returns exact rows from an explicit height index, not `build_rows`/`full_transcript_display_text`.
- If width/show-thinking/content changes invalidate heights, rebuild exact heights before exposing global row coordinates.
- The first implementation may rebuild exact heights synchronously; if that is too slow, switch to chunked exact rebuilds that do not expose approximate coordinates while incomplete.

The key architectural rule is: exact height measurement is allowed to scan blocks, but full row concatenation is not required for exact row counts.

### 6. Off-viewport display access

Replace correctness-sensitive plain-row host seams with `DisplayDocument` operations:

- ranged materialization for word motions/search/local text needs
- `copy_range` for transcript-owned copy semantics
- ranged line-break/word-boundary helpers that operate on displayed selectable text

Cost model:

- `materialize(start..end)` may render blocks intersecting that range plus minimal context; it must not call full layout unless the requested range itself spans the full transcript.
- `copy_range(range)` may materialize the selected block range and scan selected rows; huge selections are explicitly huge operations.
- line-break APIs should have a range form for virtual word motion; full break vectors are explicit full-text/export/debug operations only.

### 7. Operations allowed to scan the full transcript

Full scans/materialization should be explicit and rare:

- exporting/copying a range that actually spans the full transcript
- intentionally expensive full-text/export/debug APIs that return all rows or all break vectors
- global raw-source/all-history search, if added later, with cancellation/progress or bounded chunks
- one-time exact height measurement after width/show-thinking change, if the eager policy is chosen
- tests and debug diagnostics

These should not scan/concatenate all rows:

- normal render
- wheel scroll
- line/page cursor motions inside already-known row space
- `virtual_total_rows`
- selection paint/yank flash for visible rows
- visible block layout APIs

## Revised architecture

No internal or Lua compatibility is required. Do not preserve compatibility shims, duplicated fields, or old APIs just to reduce the diff. The rewrite should migrate fully to the model that makes invalid states unrepresentable and deletes the current parallel coordinate systems.

The recommended end-state abstractions are:

1. `WindowSurface` — owns a window's interaction role and document state.
2. `TextHit` — one chrome-aware hit-test result.
3. `DisplayDocument` — exact displayed row space with ranged materialization and copy.
4. `TextRange` / `SelectionState` — one range/selection vocabulary for bytes and display-row positions.
5. `ExactRowIndex` + `RenderedBlockCache` — exact coordinates split from bounded rendered buffers.

### A. Replace boolean interaction state with `WindowSurface`

Current window behavior is encoded by combinations of `focusable`, `selectable`, `mouse_scroll`, `cursor_line`, `selection_highlight`, `virtual_rows`, `cpos`, and selection/drag fields. That allows invalid combinations: inert chrome can focus the prompt, row viewers can carry stale byte cursors, and list rows can pretend to be text caret state.

Make the surface/document model explicit:

```rust
struct Window {
    id: WinId,
    buf: BufId,
    config: SplitConfig,
    viewport: Option<WindowViewport>,
    scroll: ScrollState,
    layout: WrappedLayout,
    surface: WindowSurface,
}

enum WindowSurface {
    EditableText(BufferDocState),
    ReadonlyText(TextDocState),
    SelectableText(TextDocState),
    List(ListDocState),
    Inert,
}

enum TextDocState {
    Buffer(BufferTextState),
    Rows(RowTextState),
}
```

Surface meanings:

- `EditableText`: prompt input or future editable buffers. Has an editable byte/source cursor and text selection.
- `ReadonlyText`: transcript, help/docs, readonly overlay bodies. Has viewer cursor/search/selection/copy but no edits.
- `SelectableText`: prompt top bar, notifications, status-like text. No keyboard focus or caret; drag-copy can select real text. Chrome clicks are inert.
- `List`: picker/menu/dialog list. Owns list index/highlight state; it does not borrow text-caret semantics.
- `Inert`: separators, pure chrome, fill bars. No focus, caret, text selection, or search.

Behavior comes from `WindowSurface`, not old booleans:

```rust
impl WindowSurface {
    fn accepts_focus(&self) -> bool;
    fn has_caret(&self) -> bool;
    fn supports_text_selection(&self) -> bool;
    fn supports_search(&self) -> bool;
    fn chrome_click_policy(&self) -> ChromeClickPolicy;
}
```

Do not keep `focusable`/`selectable`/`mouse_scroll` as parallel authorities. Delete or derive them during the migration.

### B. Use explicit text states instead of shared byte/row fields

```rust
struct BufferTextState {
    cursor: usize,
    preferred_cell_col: Option<usize>,
    selection: SelectionState<usize>,
    drag: DragState<usize>,
    yank_flash: Option<TimedRange<TextRange>>,
}

struct RowTextState {
    materialized: MaterializedRows,
    cursor: DocPosition,
    preferred_cell_col: Option<usize>,
    selection: SelectionState<DocPosition>,
    drag: DragState<DocPosition>,
    yank_flash: Option<TimedRange<TextRange>>,
}

struct ListDocState {
    selected: usize,
    scroll: RowIndex,
}
```

This should delete the dual authority of local byte `cpos` plus `virtual_rows.cursor`, byte `selection_anchor` plus row selection anchor, byte drag endpoint plus row drag endpoint, and byte yank flash plus row yank flash.

### C. Add one chrome-aware `TextHit`

Current hit-testing collapses a mouse point to a byte/position too early. Keep the semantic hit result until `WindowSurface` policy decides what to do:

```rust
struct TextHit<P> {
    row: RowIndex,
    cell_col: usize,
    kind: TextHitKind<P>,
}

enum TextHitKind<P> {
    Selectable { before: P, after: P },
    LeadingChrome { nearest: Option<P> },
    TrailingChrome { nearest: Option<P> },
    AllChrome,
    EmptyRow,
    Outside,
}

enum ChromeClickPolicy {
    Inert,
    SnapCaret,
    ExtendSelectionOnlyIfDragStartedOnText,
}
```

Mouse routing becomes:

```text
raw mouse -> TextHit -> WindowSurface policy -> focus/caret/selection action
```

Required policies:

- editable/readonly text may snap caret placement through chrome where appropriate.
- selectable-text chrome/fill clicks are inert and must not snap to distant selectable text.
- inert surfaces ignore text selection and caret movement.
- drag-copy starts from selectable text; any drag-crosses-into-text behavior must be deliberate and tested.

This replaces transcript-specific TUI pre-snapping and fixes prompt-bar chrome, code-block padding, all-chrome rows, and edge-row drag offsets through one path.

### D. Use `DisplayDocument` for displayed selectable text

Search/selection/copy operate over displayed selectable text, not raw row strings. Replace correctness-sensitive `Vec<String>` seams with displayed row data:

```rust
trait DisplayDocument {
    fn snapshot(&mut self) -> DisplaySnapshot;
    fn materialize(&mut self, range: Range<RowIndex>) -> DisplayRows;
    fn copy_range(&mut self, range: DocRange) -> CopyOutput;
}

struct DisplaySnapshot {
    generation: u64,
    total_rows: RowIndex,
}

struct DisplayRows {
    row_base: RowIndex,
    rows: Vec<DisplayRow>,
}

struct DisplayRow {
    text: String,
    spans: Vec<Span>,
    decoration: LineDecoration,
}
```

Implement this model for buffer-backed text, transcript projection, readonly overlay/dialog text, and selectable bars where applicable. Search, hit-testing, selection projection, and copy should consume `DisplayRow` metadata instead of reconstructing selectable masks from plain strings.

`MaterializedRows` remains useful as the local/absolute row-space primitive inside `RowTextState`, but it should be owned by the row text state rather than exposed as an optional virtual mode bolted onto byte cursor fields.

### E. Split exact row coordinates from rendered transcript buffers

`BlockRowIndex` should become `ExactRowIndex`: the durable coordinate authority. `BlockBufferCache` should become a bounded `RenderedBlockCache`. Do not equate “exact height known” with “rendered block buffer retained.”

```rust
struct ExactRowIndex {
    blocks: Vec<BlockRow>,
    prefix_rows: Vec<RowIndex>,
}

struct RenderedBlockCache {
    // bounded LRU/generational cache of rendered block buffers
}
```

Rules:

- exact heights are stored by block id/key and prefix-summed for `virtual_total_rows`, scrollbar math, `gg`, `G`, search origins/results, selection, and copy.
- a height measurement may render a block, but after recording height the rendered buffer may be evicted unless visible/recent/search-copy needs it.
- normal render materializes only visible rows plus bounded overscan.
- full row concatenation is allowed only for explicit export/debug/full-text operations.

### F. Use `TextRange` and `SelectionState` everywhere

Do not defer the range/selection cleanup. Since the migration can be complete, move byte and row selections to one generic shape:

```rust
struct SelectionState<P> {
    anchor: Option<P>,
    head: P,
    mode: SelectionMode,
}

enum SelectionMode {
    Char,
    Line,
}

enum TextRange {
    Bytes(std::ops::Range<usize>),
    Rows(DocRange),
}

enum RangeLayer {
    Selection,
    Search,
    YankFlash,
}
```

One projection path converts `TextRange` into visible `SelectionRange`s using `DisplayRow` metadata and the materialized row space. Render selection, search highlights, yank flash, copy dispatch, and autoscroll should all consume this vocabulary.

### G. Search is a `DisplayDocument` feature

Search should not have a separate provider hierarchy. It scans the focused searchable `WindowSurface`'s `DisplayDocument`:

```rust
struct SearchSession {
    target: WinId,
    query: String,
    direction: SearchDirection,
    matches: Vec<TextRange>,
    current: Option<usize>,
    generation: SearchGeneration,
}
```

`/` and `?` use the generalized bottom status input. Enter scans the captured target display document, stores all matches, closes the status input, and paints visible matches. `n`/`N` jump through stored matches without rescanning.

Search operates over selectable display text by default:

- non-selectable chrome, gutters, borders, and padding are ignored.
- code-block trailing background/padding is not searchable.
- prompt-bar dash/fill chrome is not searchable.
- hidden/collapsed content is not searched unless part of the current visible display document.
- raw-source/all-history search is a separate future mode.

## Revised migration plan

Use larger logical commits. It is acceptable for a phase to be several thousand lines if it removes dual authority and leaves the product coherent. Do not spend effort making tiny intermediate scaffolding commits green if the intermediate state preserves the old broken model.

### Phase 0: remove debug noise, add test hooks, and lock behavior tests

Remove staged debug `eprintln!` calls first. They obscure test output and make search/repeat-key timing harder to reason about.

Add test/perf instrumentation early:

- full row builds
- rendered block count
- exact-height measured block count
- range-materialized block count

Regression coverage should be TUI-level whenever the bug was visible through real event/render routing. Unit tests in `smelt-edit`/`smelt-buffer` are still useful, but they are not substitutes for the original broken/fixed behavior table.

Existing TUI coverage to preserve:

- `crates/tui/src/app/test_harness.rs:3475` — `transcript_vim_gg_g_and_count_g_use_virtual_rows` covers exact virtual-row `gg`, `G`, and count-`G` navigation.
- `crates/tui/src/app/test_harness.rs:3494` — `transcript_vim_visual_yank_copies_virtual_range` covers Vim visual yank through `copy_virtual_range` and virtual yank flash expiry.
- `crates/tui/src/app/test_harness.rs:3524` — `transcript_vim_visual_char_starts_at_cursor` covers visual selection starting at the cursor instead of the top of the transcript, plus mouse-down clearing stale visual state.
- `crates/tui/src/app/test_harness.rs:3588` — `wheel_scroll_in_visual_mode_preserves_cursor_screen_row` covers visual-mode wheel scrolling preserving screen-row intent.
- `crates/tui/src/app/test_harness.rs:3620` — `mouse_drag_clears_visual_line_mode` covers mouse drag exiting Vim visual-line mode.
- `crates/tui/src/app/test_harness.rs:3654` — `transcript_shift_selection_copy_copies_virtual_range` covers shift-selection copy from a virtual transcript.
- `crates/tui/src/app/test_harness.rs:3669` — `transcript_triple_click_event_pipeline_yanks_clicked_display_line` covers the real event pipeline for transcript line yanking.
- `crates/tui/src/app/mouse.rs:645` — `transcript_click_after_tail_render_lands_on_clicked_screen_row` covers clicks in a tail-projected virtual transcript.
- `crates/tui/src/app/mouse.rs:733` — `transcript_drag_after_tail_render_starts_from_clicked_row` covers drag anchors in a tail-projected virtual transcript.
- `crates/tui/src/app/mouse.rs:796` — `virtual_transcript_drag_renders_cursor_and_selection_while_captured` covers rendering selection while mouse capture freezes materialization.
- `crates/tui/src/app/mouse.rs:839` — `transcript_drag_while_streaming_keeps_clicked_anchor` covers streaming appends during drag.
- `crates/tui/src/app/mouse.rs:904` — `transcript_click_uses_local_row_in_tail_projection` covers local-row hit-testing after virtual row-base projection.

Missing or still-required TUI regressions from the bug table:

Selection/cursor semantics:

1. Full-width all-chrome blank row paints exactly one visible selected cell and does not copy fake spaces.
2. Real user/exec block top/bottom padding rows paint exactly one visible selected cell.
3. Thinking block empty row paints after `│ `, not over the chrome prefix.
4. Code-block click past EOL snaps to the end of the actual code text.
5. Code-block drag/select through right padding does not copy padding spaces.
6. Virtual mouse drag from col 0 to char col 3 copies/highlights four chars, with render highlight, copied text, and yank flash agreeing.
7. Double-click word: highlighted range, copied range, and yank flash range are identical.
8. TUI transcript drag near viewport edge uses the same row as edit hit-test; no duplicate TUI snapping offset.
9. Prompt top-bar click on non-selectable `────` chrome does not focus the prompt, does not move prompt/bar cursor state, and does not snap to the right-side selectable group.
10. Prompt top-bar drag that starts on selectable text copies only that text and uses the bar selection range, not the prompt input cursor.
11. Prompt bottom-bar separator click is inert: no prompt focus, no cursor move, no selection, no copy.
12. Picker/resume-preview materialized rows still scroll/highlight correctly after the surface/document refactor.

Virtualization shape:

1. `virtual_total_rows` or equivalent exact total-row query does not call `build_rows`/`full_transcript_display_text`.
2. `gg` and `G` use exact row counts without full row concatenation.
3. Wheel scroll on a large transcript materializes only viewport plus overscan block ranges.
4. ranged display materialization touches only requested/intersecting blocks.
5. copy of a small row range materializes only selected blocks.
6. `visible_blocks` does not force full transcript layout.
7. Scrollbar thumb and scroll position use the exact row index, not estimates or materialized-buffer length.

Search behavior:

1. `/` opens the bottom status input when a searchable readonly viewer is focused.
2. `/` does not steal focus from editable text.
3. Enter computes all matches, closes the status input, and highlights visible matches.
4. Off-viewport matches highlight when scrolled into view.
5. `n` and `N` jump through precomputed matches without rescanning.
6. `Esc` cancels search and clears highlights without implicitly changing visual selection.
7. Search ignores non-selectable chrome and code-block padding.
8. Search targets the currently focused overlay/dialog/window, not always the transcript.
9. Search match ranges survive rematerialization of the target viewport and are cleared on incompatible target generation changes.

### Phase 1: window surface and text interaction rewrite

Do the full interaction/document-state migration in one coherent phase. Do not keep compatibility fields as parallel truth.

Scope:

- introduce `WindowSurface` and move interaction behavior/state into it.
- replace `focusable`, `selectable`, and `mouse_scroll` authority with `WindowSurface` methods.
- introduce `BufferTextState`, `RowTextState`, `ListDocState`, and `TextDocState`.
- delete `virtual_rows: Option<VirtualRowsState>` as a bolted-on mode; row viewers use `WindowSurface::ReadonlyText(TextDocState::Rows(...))`.
- delete or privatize stale byte cursor/selection/drag/yank state for rows/list/inert surfaces.
- introduce `SelectionState<P>`, `TextRange`, and `RangeLayer` as the core text-range vocabulary.
- introduce `TextHit` / `TextHitKind` and make mouse selection/caret placement use hit kind plus surface policy.
- route focus-on-click, caret ownership, search eligibility, text-selection eligibility, and chrome-click policy through `WindowSurface`.
- make prompt top-bar chrome/fill clicks inert; selectable text drag-copy still works.
- make bottom prompt bar `WindowSurface::Inert` unless explicitly changed to selectable text.
- change blank user/exec chrome rows to semantic empty rows with row background fill.
- change code-block trailing background/padding so it is not selectable/searchable/cursor-addressable text.
- if real pad cells are required for a right border, emit them as non-selectable chrome.
- replace transcript-specific TUI snapping with edit/window hit-testing over display row spans/decorations.
- keep TUI responsible for dispatch/capture/focus/clipboard, not cell snapping.

This phase should eliminate the prompt-bar `────` bug, the code-block padding cursor bug, all-chrome selection bugs, and duplicate transcript snapping offsets while also deleting the old dual byte/row window state.

### Phase 2: display document and transcript virtualization rewrite

Do display-document metadata, exact row indexing, and bounded transcript projection together. Do not land a halfway state where exact totals exist but normal render/range/copy still full-materialize common small operations.

Scope:

- introduce `DisplayDocument`, `DisplaySnapshot`, `DisplayRows`, and `DisplayRow`.
- implement display documents for buffer-backed text, transcript projection, readonly overlay/dialog text, and selectable bars where applicable.
- make search, hit-testing, selection projection, and copy consume `DisplayRow` text/spans/decorations.
- split transcript `ExactRowIndex` from bounded `RenderedBlockCache`.
- implement the invalidation matrix for append, block replacement, width change, show-thinking change, and theme changes.
- make exact heights the authority for total rows, scrollbar math, `gg`, `G`, search origins/results, selection ranges, and copy ranges.
- make normal render choose a row window from the exact row index, then render/cache only intersecting blocks.
- keep visible projection as the only data copied into the edit `Buffer` during normal transcript render.
- make ranged display materialization and copy materialize only intersecting/selected blocks.
- make `visible_blocks` use the visible projection only and never force full transcript layout.
- reserve full row concatenation for explicit export/debug/full-text operations only.

Target flow:

```text
scroll/search/copy target -> DisplayDocument snapshot -> ExactRowIndex -> bounded DisplayRows -> render/search/copy
```

Never:

```text
scroll/search/copy target -> build all rows -> slice result
```

Start with synchronous exact height rebuild if that is simplest. If it is too slow, replace it with chunked exact rebuild, but do not expose approximate rows as user-visible coordinates.

### Phase 3: generic status-input search over display documents

Build search as a complete user feature over `DisplayDocument`, not as a separate provider hierarchy.

Scope:

- generalize the existing `:` cmdline overlay into a status input with `Command` and `Search` modes.
- capture the target `WinId` when `/` or `?` opens search.
- open search only for focused searchable `WindowSurface`s.
- submit the query with Enter and compute all matches for that target.
- store search session state with target, query, direction, generation, matches, and current match index.
- paint all visible matches; store off-viewport matches as `TextRange`s for later paint.
- implement `n`/`N` over the precomputed match vector.
- scroll/materialize the target window when jumping to a match.
- clear search on `Esc`, target close, or incompatible target generation change.

Search rules:

- normal readonly buffers search their display document directly.
- virtual/transcript windows use exact row index plus bounded display materialization.
- searchable text excludes non-selectable chrome, borders, gutters, and visual padding.
- no disk index; no full row concatenation.
- the first implementation may scan synchronously on Enter, but keep the scan isolated enough to chunk/cancel later without placeholder async machinery.

### Phase 4: deletion and simplification pass

After the new model is green, delete the old model rather than preserving shims.

Scope:

- delete old `virtual_rows` APIs and compatibility wrappers that are no longer used.
- delete old `rows_for`/`breaks_for` APIs if display documents replace them; keep only explicit full-text/export APIs that are intentionally expensive.
- delete old `focusable`/`selectable`/`mouse_scroll` authority if not already removed.
- delete old byte/doc projection duplication after `TextRange` projection is authoritative.
- delete or update Lua APIs freely; no Lua compatibility is required.
- simplify tests around `WindowSurface`, `DisplayDocument`, `TextHit`, and `TextRange`.

## Design decisions after code review

- Use `WindowSurface` as the single owner of both interaction role and document state.
- Use `WindowSurface::{EditableText, ReadonlyText, SelectableText, List, Inert}` to make invalid interaction combinations unrepresentable.
- Delete compatibility state internally; no parallel `focusable`/`selectable`/`mouse_scroll` authority, no parallel byte and row cursor/selection authorities.
- Add `TextHit` / `TextHitKind`; do not collapse hit-testing into a byte offset before surface policy is applied.
- Use `DisplayDocument`, `DisplayRows`, and `DisplayRow` because search/selection/copy operate over displayed selectable text.
- Use `TextRange` and `SelectionState<P>` in both byte-backed and row-backed text states.
- Keep `MaterializedRows` as the local/absolute row-space primitive inside `RowTextState`, not as an optional virtual mode on `Window`.
- Keep transcript copy projection-owned, but route it through display-document/ranged copy semantics.
- Split exact row-height/index state from rendered block buffer retention.
- User-visible row counts and coordinates must be exact; no approximate `gg`, `G`, scrollbar, search, selection, or copy coordinates.
- Do not model Smelt transcript virtualization as a tmux-style full cell grid; keep structured block history plus exact row index and bounded rendered block cache.
- Do not let normal render/scroll/search/copy of small ranges concatenate the whole transcript.
- Code-block trailing background/padding is chrome, not selectable/searchable text.
- Prompt-bar dashes/fill are chrome: clicking them must not focus the prompt or snap the selection cursor to nearby selectable status text.
- Reuse/generalize the bottom `:` cmdline overlay for `/` and `?` search input.
- Search should feel nvim-like (`/`, `?`, highlighted submitted matches, `n`/`N`) while using tmux-like exact backing coordinates and viewport-bounded redraw.
- Search is a consumer of display documents, not a separate provider hierarchy.
- Search computes and stores all matches on submit so `n`/`N` can jump quickly without rescanning.
- Search default is visible selectable display text; raw-source/all-history search is a separate future mode.
- Do not add a disk search index.

## Open questions before implementation

1. Should `?` for backward search be implemented in the same search phase as `/`? Current recommendation: yes, since the status-input/search direction model needs it anyway.
2. Should `Esc` cancel only search highlights, or also leave the viewer's visual selection if one exists? Current recommendation: `Esc` in active search clears search; Vim visual `Esc` clears visual selection.
3. What in-memory limit should `RenderedBlockCache` use for long transcripts, and should search-scanned blocks enter the same LRU as render-scanned blocks?
4. Which full-text/export/debug APIs are intentionally expensive and allowed to force full materialization?
5. Are there any Lua APIs worth redesigning around `WindowSurface`/`DisplayDocument`, or should they be deleted until a new public API is needed?
