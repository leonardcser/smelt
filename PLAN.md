# Plan: Virtual Documents and Lazy Transcript Rendering

## Purpose

Huge restored sessions are slow because the first rendered frame eagerly rebuilds the
entire transcript display buffer and eagerly calls every restored tool render hook.
This plan describes a long-term architecture that makes restored sessions fast by
rendering only the visible transcript range, while preserving current behavior for
scrolling, selection, copy/yank, Vim mode, Lua APIs, tool rendering, resize anchoring,
and future plugin/Lua virtualized views.

This is intentionally a broad architecture plan, not an implementation patch. The
core constraint is zero user-visible regression: existing buffer-backed windows must
continue behaving as they do today, and transcript compatibility APIs that expose the
full rendered transcript must keep working, even if they become explicit
materialization points.

## Current root cause

The slow path is architectural, not a remaining local hot function.

Relevant current flow:

1. Session restore rebuilds tool states without rendered caches.
   - `crates/tui/src/app/history.rs:399`
2. Every frame's transcript sync calls `project_transcript_buffer`.
   - `crates/tui/src/app/render_loop.rs:163`
3. `project_transcript_buffer` eagerly calls `prerender_tool_blocks` before layout.
   - `crates/tui/src/app/transcript.rs:355`
   - `crates/tui/src/app/transcript.rs:369`
4. `prerender_tool_blocks` walks every `Block::ToolCall` and calls Lua render hooks
   for every width-stale restored tool.
   - `crates/tui/src/app/transcript.rs:395`
   - `crates/tui/src/app/transcript.rs:405`
5. Transcript projection ensures and stitches every block into a single backing
   `Buffer`.
   - `crates/tui/src/content/transcript_buf.rs:90`
   - `crates/tui/src/content/transcript_buf.rs:142`
   - `crates/tui/src/content/transcript_buf.rs:162`
6. `Window::render` paints only visible rows, but it receives an already-materialized
   buffer and cached wrap layout.
   - `crates/edit/src/window.rs:1613`

The result: the first frame after resume pays for every historical block, every
historical tool render cache miss, and every rendered transcript row, even though the
terminal can display only a viewport-sized suffix.

## Additional architectural limit discovered

The current window/transcript row model uses `u16` for scroll and total row state in
many places. That is a hidden scalability ceiling independent of CPU time.

Examples:

- `Window.scroll_top: u16` and `Window.cursor_row: u16`.
  - `crates/edit/src/window.rs:271`
  - `crates/edit/src/window.rs:285`
- `WindowViewport.total_rows: u16`.
  - `crates/edit/src/window.rs:116`
- `TranscriptProjection::LayoutEntry.rows: u16`.
  - `crates/tui/src/content/transcript_buf.rs:31`
- Transcript block layout and scroll totals clamp to `u16`.
  - `crates/tui/src/content/transcript_buf.rs:62`
  - `crates/tui/src/content/transcript_buf.rs:505`
- Lua cursor/scroll setters clamp inputs to `u16`.
  - `crates/tui/src/lua/api/win.rs:220`
  - `crates/tui/src/lua/api/win.rs:431`

The virtualized implementation should use a large row index internally (`usize` or
`u64`) and convert to `u16` only at terminal geometry/paint boundaries.

## Goals

- First frame after restoring a large session should render only the visible
  transcript range plus bounded overscan.
- Restored tool render hooks should be invoked only for visible/overscan tool nodes,
  not every historical tool.
- Jumping to tail, jumping to a block, scrollbar drag, and direct row scroll should
  remain immediate.
- Existing buffer-backed windows, plugin windows, overlays, picker/list windows,
  prompt behavior, and Lua APIs should continue to work.
- Full-text APIs should remain exact, but they may explicitly materialize the full
  virtual document.
- The new abstraction should be generic enough to support future Lua/plugin virtual
  views and to eventually replace eager picker/list buffers.

## Non-goals for the first implementation

- Do not rewrite transcript rendering into Lua.
- Do not replace `smelt.layout`; evolve it.
- Do not require every renderer to support true row-range rendering immediately.
  Initially, node renderers may render a whole node and slice visible rows. The
  critical boundary is that the window/document interface is range-based.
- Do not remove `Buffer`; it remains the backing store for editable/plugin windows
  and is wrapped by a compatibility document adapter.

## Recommended architecture

### 1. Add a generic document abstraction

Current model:

```text
Ui
  Window
    BufId
      Buffer { all lines, extmarks, source, parser, copier }
```

Target model:

```text
Ui
  Window
    DocumentRef
      BufferDocument(Buffer)          // compatibility adapter
      TranscriptDocument(provider)    // lazy virtual transcript
      Future Lua/List/File documents
```

The trait should live in `crates/edit`, because `Window` owns scrolling, cursor,
selection, mouse handling, rendering, scrollbars, and events. Transcript-specific
providers should live in `crates/tui`.

Sketch:

```rust
pub type RowIndex = u64;

pub enum DocPos {
    BufferByte(usize),
    RowCol { row: RowIndex, col: usize },
    Transcript(TranscriptPos),
}

pub struct DisplayRow {
    pub text: String,
    pub highlights: Vec<Span>,
    pub decoration: LineDecoration,
    pub virtual_text: Vec<VirtualText>,
}

pub trait Document {
    fn revision(&self) -> u64;
    fn total_rows(&mut self, width: u16, theme: &Theme) -> RowIndex;
    fn rows(
        &mut self,
        range: Range<RowIndex>,
        width: u16,
        theme: &Theme,
    ) -> Vec<DisplayRow>;

    fn row_to_pos(&mut self, row: RowIndex, col: usize, width: u16, theme: &Theme) -> DocPos;
    fn pos_to_row_col(&mut self, pos: &DocPos, width: u16, theme: &Theme) -> (RowIndex, usize);

    fn copy_range(&mut self, start: &DocPos, end: &DocPos) -> CopyOutput;
    fn word_range_at(&mut self, pos: &DocPos) -> Option<(DocPos, DocPos)>;
    fn line_range_at(&mut self, pos: &DocPos) -> Option<(DocPos, DocPos)>;
    fn block_range_at(&mut self, pos: &DocPos) -> Option<(DocPos, DocPos)>;
}
```

`BufferDocument` adapts the current `Buffer` APIs exactly:

- rows come from `Buffer::lines`, `highlights_at_into`, `decoration_at`, and
  `virtual_text_at_into`.
- positions are existing editable byte offsets.
- copy is `Buffer::copy_range`.
- wrapping remains `WrappedLayout` initially.

This first adapter step lets the new interface be introduced with minimal behavior
change before transcript virtualization begins.

### 2. Make `Window` document-aware while preserving `Buffer` behavior

`Window` currently combines several responsibilities:

- width-aware wrap layout caching via `WrappedLayout`.
  - `crates/edit/src/window.rs:384`
- scroll anchoring and resize restore.
  - `crates/edit/src/window.rs:446`
  - `crates/edit/src/window.rs:469`
- cursor byte/row projection.
  - `crates/edit/src/window.rs:537`
  - `crates/edit/src/window.rs:562`
- mouse click/drag selection.
  - `crates/edit/src/window.rs:1143`
  - `crates/edit/src/window.rs:1215`
- Vim key handling and copy/yank offsets.
  - `crates/edit/src/window.rs:1371`
- paint of rows, highlights, selection, virtual text, gutter, scrollbar, cursor.
  - `crates/edit/src/window.rs:1613`

The migration should not duplicate these paths for transcript. Instead, split the
parts that assume a concrete `Buffer` into a document adapter layer.

Initial target:

```text
Window state:
  scroll_top: RowIndex
  scroll_left: u16
  cursor: DocPos
  selection_anchor: Option<DocPos>
  drag_endpoint: Option<DocPos>
  follow_tail: bool or ScrollTarget::Tail
```

Paint still takes a `GridSlice`, but it asks the document for only:

```text
visible = scroll_top .. scroll_top + viewport_height
rows = document.rows(visible ± overscan)
```

For `BufferDocument`, output should be identical to today.

### 3. Replace `u16` row state with large row indices

Required internal widening:

- `Window.scroll_top`
- `Window.cursor_row`
- `WindowViewport.total_rows`
- scroll-link group state
- scroll callback payload internals
- transcript block layout row counts/starts
- Lua cursor/scroll setters/getters
- list/picker cursor helpers that currently clamp rows to `u16`

Terminal dimensions remain `u16`; only document row positions grow.

Lua compatibility:

- Existing Lua numbers can represent large integers as `mlua::Integer`.
- Preserve field names: `top`, `total`, `max`, `viewport`, `overflow`, etc.
- Avoid clamping `total`, `top`, and `max` to `u16` for virtual docs.
- Event payload `Payload::Scroll { top, follow }` currently carries `u16`.
  Widen it or add a `top_large`/integer path before virtual transcript lands.
  - `crates/edit/src/callback.rs:109`
  - `crates/edit/src/lib.rs:1991`

### 4. Implement `TranscriptDocument` as hierarchical row virtualization

Do not make transcript a flat row callback provider. Keep semantic nodes.

```text
TranscriptDocument
  history generation
  width
  show_thinking
  theme revision
  nodes: Vec<TranscriptNode>
  height_index: prefix sums / Fenwick tree
```

Each node:

```text
struct TranscriptNode {
    id: BlockId,
    key: LayoutKey,
    estimated_height: RowIndex,
    exact_height: Option<RowIndex>,
    row_cache: Option<NodeRows>,
}
```

Global row lookup:

```text
global row -> height_index.lower_bound(row) -> (node_index, local_row)
node index -> prefix_sum(node_index) -> first global row
```

Visible render:

```text
1. Window computes visible range from scroll_top and viewport height.
2. TranscriptDocument maps visible range to node spans.
3. Only intersecting nodes are rendered or measured.
4. Tool Lua render hooks are invoked only if a tool node intersects visible/overscan.
5. Rows are returned to Window for paint.
```

This replaces the current eager path:

- `TranscriptProjection::project` full-buffer stitch.
  - `crates/tui/src/content/transcript_buf.rs:90`
- `BlockBufferCache::ensure_many` over all blocks.
  - `crates/tui/src/content/block_buffers.rs:35`
- `TuiApp::prerender_tool_blocks` over all tool calls.
  - `crates/tui/src/app/transcript.rs:395`

### 5. Preserve current full-materialization APIs as explicit compatibility paths

Some existing APIs intentionally expose the whole rendered transcript or whole buffer.
They must remain exact.

Compatibility materialization points:

- `smelt.transcript.text()`
  - `crates/tui/src/lua/api/transcript.rs:17`
  - Uses `TuiApp::full_transcript_display_text` today.
  - `crates/tui/src/app/transcript.rs:131`
- `UiHost::rows_for(TRANSCRIPT_WIN)`
  - `crates/tui/src/app/ui_host.rs:44`
- `UiHost::breaks_for(TRANSCRIPT_WIN)`
  - `crates/tui/src/app/ui_host.rs:56`
- `TranscriptProjection::build_rows` and `line_breaks`
  - `crates/tui/src/content/transcript_buf.rs:383`
  - `crates/tui/src/content/transcript_buf.rs:433`

New behavior:

- normal render path does not call these APIs;
- if a plugin/user calls them, the transcript document materializes all rows exactly;
- cached full materialization remains keyed by generation/width/show_thinking/theme.

Add new virtual-friendly APIs instead of changing old semantics:

```lua
smelt.transcript.visible_blocks()
smelt.transcript.rows(start, count)
smelt.transcript.block_at_row(row)
win:visible_range()
```

`smelt.transcript.blocks()` should remain exact for compatibility, or grow an
opt-in estimated mode later.

## Feature/regression inventory

This section lists every currently relevant behavior found in the code review and
what the virtual architecture must preserve.

### A. Tail follow

Current behavior:

- Transcript window starts with `follow_tail = true`.
  - `crates/tui/src/app.rs:539`
- `Ui::apply_tail_follow` pins opted-in windows to the buffer tail unless selection,
  Visual mode, or active drag freezes it.
  - `crates/edit/src/lib.rs:1929`
  - `crates/edit/src/lib.rs:1949`
- Transcript uses a `u16::MAX` sentinel because its backing buffer is rebuilt
  mid-frame.
  - `crates/tui/src/app/render_loop.rs:32`
  - `crates/tui/src/app/render_loop.rs:37`

Virtual requirement:

- Replace sentinel with explicit state:

```rust
enum ScrollTarget {
    Row(RowIndex),
    Tail,
}
```

- Tail resolution uses `document.total_rows(width) - viewport_rows`.
- Selection/Visual/drag freezes tail exactly as today.
- Streaming append invalidates only suffix node heights and keeps tail pinned.

### B. Scroll operations

Current behavior:

- Mouse wheel pans viewport and preserves cursor screen row.
  - `crates/edit/src/lib.rs:620`
  - `crates/edit/src/window.rs:1546`
- Programmatic `win:scroll(n)` does the same.
  - `crates/tui/src/lua/api/win.rs:431`
- Scrollbar drag maps thumb position to `scroll_top`.
  - `crates/edit/src/lib.rs:1724`
- Scroll links mirror `scroll_top` and `scroll_left` across windows.
  - `crates/edit/src/lib.rs:219`
- Scroll events fire when `(scroll_top, follow_tail)` changes.
  - `crates/edit/src/lib.rs:1989`

Virtual requirement:

- Use large row indices for vertical scroll.
- Keep `scroll_left` as `u16` unless horizontal virtualization is added later.
- Scrollbar math must handle large `total_rows` without overflow; compute ratios with
  `u128` or saturating helpers.
- Scroll links need to either:
  - link absolute row numbers for homogeneous documents, or
  - eventually support normalized/anchor-based linking for heterogeneous virtual docs.
  For the first migration, preserve current absolute-row behavior.
- Scroll event payload must no longer truncate large `top` values.

### C. Resize anchoring

Current behavior:

- Generic windows stamp `(changedtick, logical_row, byte)` at viewport top.
  - `crates/edit/src/window.rs:440`
- Resize restores the same logical row/chunk when width/wrap changes.
  - `crates/edit/src/window.rs:463`
- Transcript projection already has block-level resize anchoring.
  - `crates/tui/src/content/transcript_buf.rs:116`
  - `crates/tui/src/content/transcript_buf.rs:325`

Virtual requirement:

- Introduce a document anchor abstraction:

```rust
enum ViewAnchor {
    Buffer { revision: u64, row: usize, byte: usize },
    Transcript { block_id: BlockId, local_row: RowIndex, fallback_fraction: f64 },
}
```

- Width/show_thinking/theme changes restore `scroll_top` from the anchor.
- If a block height changes from estimated to exact, preserve the anchor block/local
  row instead of raw absolute row.

### D. Cursor navigation

Current behavior:

- Cursor is byte-position based for buffers.
  - `crates/edit/src/window.rs:263`
- Cursor row/col are derived through `Buffer::display_byte_pos` and `WrappedLayout`.
  - `crates/edit/src/window.rs:537`
- Vim and arrows move through text and keep cursor visible.
  - `crates/edit/src/window.rs:1371`
  - `crates/edit/src/window.rs:1474`
- List leaves drive row cursor via `jump_to_row`.
  - `crates/edit/src/window.rs:709`
  - `crates/tui/src/lua/ui_ops.rs:255`

Virtual requirement:

- Represent cursor as `DocPos`, not always byte offset.
- For `BufferDocument`, keep exact byte semantics.
- For `TranscriptDocument`, support at minimum row/col cursor positions, vertical
  motion, home/end/top/bottom, and visible cursor painting.
- List/picker APIs should move to large row indices while keeping current behavior.

### E. Selection, mouse drag, copy/yank, yank flash

Current behavior:

- Mouse down/drag/up uses row/col -> byte offsets and returns selected byte range.
  - `crates/edit/src/window.rs:1143`
  - `crates/edit/src/window.rs:1215`
  - `crates/edit/src/window.rs:1258`
- Selection paint uses `SelectionRange { line, col_start, col_end }`.
  - `crates/buffer/src/buffer.rs:240`
- Transcript has a custom copier that preserves kill-ring raw text and external
  clipboard display text.
  - `crates/tui/src/content/transcript_buf.rs:520`
- Transcript copy honors non-selectable spans, `copy_as`, `source_text`, soft-wrap
  merging, and `copy_continuation`.
  - `crates/tui/src/content/transcript_buf.rs:541`
- Yank flash is painted via transcript selection highlights.
  - `crates/tui/src/app/transcript.rs:473`

Virtual requirement:

- Selection range should become `DocPos..DocPos`.
- `Document::copy_range` owns extraction so transcript does not need a full joined
  `buf.text()`.
- `Document::selection_rows(visible_range, selection)` can produce visible
  `SelectionRange` rows for paint.
- Preserve transcript copy semantics by reusing/adapting `copy_byte_range` logic over
  virtual rows.
- Yank flash must store document positions or a stable transcript position, not only
  full-buffer byte offsets, for virtual transcript. Buffer-backed windows can keep
  byte ranges.

### F. Word, line, and block selection

Current behavior:

- Double-click selects table cells or WORD ranges.
  - `crates/edit/src/window.rs:1172`
  - `crates/edit/src/window.rs:820`
- Triple-click selects structured blocks or source lines.
  - `crates/edit/src/window.rs:1188`
  - `crates/edit/src/window.rs:881`
- Soft vs hard breaks are supplied through `UiHost::breaks_for`.
  - `crates/edit/src/lib.rs:2072`
  - `crates/tui/src/app/ui_host.rs:56`

Virtual requirement:

- Move these into document-level operations:

```rust
word_range_at(pos)
line_range_at(pos)
block_range_at(pos)
```

- `BufferDocument` can keep existing text/break helpers.
- `TranscriptDocument` computes ranges from visible/materialized rows and row
  decorations. Full `breaks_for` remains a compatibility materialization path.

### G. Vim mode

Current behavior:

- Readonly buffers run Vim edits against scratch `buf.text()` and discard writes.
  - `crates/edit/src/window.rs:1381`
- Visual mode, selection anchor, and cursor clamping assume byte offsets into text.
  - `crates/edit/src/window.rs:720`
  - `crates/edit/src/window.rs:763`

Virtual requirement:

- Keep byte-offset Vim unchanged for `BufferDocument`.
- For virtual transcript, implement the subset used by transcript/content mode via
  document motions and document selections.
- If full text-object parity is not immediately possible, force materialization for
  unsupported Vim text objects rather than silently changing behavior.

### H. Rendering details

Current behavior in `Window::render` includes:

- gutters and pad offsets;
- row fill backgrounds;
- cursor line and selection highlight;
- spans with `selectable`, `copy_as`, `hl_eol`, `on_cursor_row`;
- per-row `LineDecoration` (`soft_wrapped`, `copy_continuation`, `source_text`,
  `source_line`, `pre_formatted`);
- virtual text;
- horizontal scroll;
- wide-char handling;
- scrollbar paint.

Important code:

- `crates/edit/src/window.rs:1613`
- `crates/buffer/src/buffer.rs:176`
- `crates/buffer/src/buffer.rs:228`

Virtual requirement:

- `DisplayRow` must carry enough metadata to paint exactly as `Buffer` rows do.
- Gutter providers currently take `&Buffer`. Either:
  - adapt gutters to a `DocumentGutter` trait, or
  - keep line-number gutters only on `BufferDocument` initially and provide transcript
    row/source-line metadata directly in `DisplayRow`.
- `LineDecoration::pre_formatted` and soft-wrap semantics must survive node rendering.

### I. Lua buffer and window APIs

Current behavior:

- `Buf:lines()` returns all backing lines.
  - `crates/tui/src/lua/api/buf.rs:228`
- `Buf:line(i)` reads a single materialized line.
  - `crates/tui/src/lua/api/buf.rs:268`
- `Buf:source()` exposes source text.
  - `crates/tui/src/lua/api/buf.rs:198`
- `Win:buf()` returns the backing buffer.
  - `crates/tui/src/lua/api/win.rs:160`
- `Win:scroll()` reports buffer line count.
  - `crates/tui/src/lua/api/win.rs:392`
- `Win:cursor()` and `Win:move_cursor()` clamp to `u16` row values.
  - `crates/tui/src/lua/api/win.rs:217`
  - `crates/tui/src/lua/api/win.rs:250`

Virtual requirement:

- Existing plugin windows remain buffer-backed and unchanged.
- Built-in transcript may still return a compatibility buffer handle, but that buffer
  should not be required for normal rendering.
- Add document-aware APIs rather than overloading `Buf` semantics too far:

```lua
win:scroll()             -- document total/top for virtual docs
win:visible_range()
smelt.transcript.rows(start, count)
smelt.transcript.text()  -- exact full materialization
```

- Do not break plugins that call `smelt.win.transcript():buf():lines()`. That path can
  materialize the transcript compatibility buffer, but it must not run during normal
  frame rendering.

### J. Lua renderers, paint leaves, overlays

Current behavior:

- Per-window Lua renderers mutate backing buffers before transcript/input sync.
  - `crates/tui/src/app/render_loop.rs:79`
  - `crates/tui/src/lua/api/win.rs:487`
- `smelt.paint` is visible/cell-paint oriented and already receives a slice.
  - `crates/tui/src/lua/api/paint.rs:178`
- Overlay layout leaves are windows or paint ids; natural sizes can be static or
  live-updated.
  - `crates/tui/src/lua/api/overlay_layout.rs:237`

Virtual requirement:

- Do not regress paint leaves; they are already visible-only but not document-based.
- Per-window Lua renderers remain buffer-backed initially.
- Future virtual Lua documents should use coarse row-range callbacks, not per-cell
  paint callbacks and not one Lua call per row.

### K. Tool render callbacks and `smelt.layout`

Current behavior:

- Tool render callbacks return `smelt.layout` (`BlockLayout`) trees.
  - `crates/core/src/lua/runtime.rs:1253`
  - `crates/core/src/lua/api/layout.rs:61`
- Layout leaves include `Buf`, `Diff`, `FileView`, and cached diff.
  - `crates/core/src/content/block_layout.rs:40`
- Rendered layouts are replayed into tool blocks and capped.
  - `crates/tui/src/content/transcript_parsers/tools.rs:346`
  - `crates/tui/src/content/transcript_parsers/tools.rs:470`

Virtual requirement:

- Keep `smelt.layout` as the declarative retained IR.
- Tool node rendering should call Lua only when the node intersects visible/overscan.
- Rendered layout caches should be per-tool-node and keyed by width/status/output hash.
- `Leaf::Buf` is eager because plugins write a whole buffer; keep compatibility, but
  add future lazy layout leaves for large text/diff/file/custom providers.

### L. Picker/list windows

Current behavior:

- Picker buffers eagerly write every item.
  - `crates/tui/src/picker.rs:154`
  - `crates/tui/src/picker.rs:308`
- Built-in list keymaps use backing buffer line counts and `u16` row cursor.
  - `crates/tui/src/lua/ui_ops.rs:255`

Virtual requirement:

- Do not migrate picker in the first transcript step unless needed.
- Design the document abstraction so picker/list can be the second consumer.
- Before migrating picker, widen its row math and selection payloads to avoid new
  `u16` ceilings.

### M. Prompt/input buffers

Current behavior:

- Prompt is source/parser-backed and uses special install/mutation seams.
- Instructions require buffer-wide prompt source swaps to go through
  `PromptState::install_source` or equivalent.
- Prompt display and selection are synchronized separately.
  - `crates/tui/src/content/prompt_buf.rs:61`
  - `crates/tui/src/app/render_loop.rs:222`

Virtual requirement:

- Do not virtualize prompt in this project.
- Ensure generic `BufferDocument` preserves parser/source/copy semantics.
- Do not introduce raw `Buffer::source` writes.

### N. Theme, `show_thinking`, settings changes

Current behavior:

- Transcript projection cache invalidates for theme.
  - `crates/tui/src/content/transcript_buf.rs:83`
- Transcript layout key includes `show_thinking`.
  - `crates/tui/src/content/transcript_buf.rs:35`

Virtual requirement:

- Node caches are keyed by:
  - transcript generation;
  - width;
  - `show_thinking`;
  - theme/style revision;
  - block content hash;
  - sidecar/layout revision;
  - view state.
- Changing these invalidates only affected node rows/heights where possible.

## Architecture failure modes / edge cases discovered in deeper review

These are the places most likely to produce subtle regressions if transcript becomes
virtual but the surrounding UI remains buffer-shaped.

1. **Frame-order invariants**
   - Current frame order is layout, Lua per-window renderers, transcript sync, input
     sync, final viewport stamping, then paint.
     - `crates/tui/src/app/render_loop.rs:58`
     - `crates/tui/src/app/render_loop.rs:79`
     - `crates/tui/src/app/render_loop.rs:86`
     - `crates/tui/src/app/render_loop.rs:99`
   - A virtual document must not require a painted frame before it can answer row
     requests. Width, viewport height, theme revision, and scroll target must be enough.
   - Per-window Lua renderers remain buffer-backed and must continue running before
     `BufferDocument` rows are requested.

2. **Mouse routing, capture, and drag autoscroll**
   - `Ui::dispatch_event` owns wheel routing, scrollbar drag, overlay chrome drag, and
     modal mouse behavior.
     - `crates/edit/src/lib.rs:1530`
     - `crates/edit/src/lib.rs:1550`
     - `crates/edit/src/lib.rs:1615`
   - `TuiApp::handle_mouse` owns app-focus promotion, pointer callbacks, and built-in
     prompt/transcript/selectable-leaf selection.
     - `crates/tui/src/app/mouse.rs:73`
     - `crates/tui/src/app/mouse.rs:119`
     - `crates/tui/src/app/mouse.rs:141`
   - `Ui::resolve_split_mouse` latches captured leaves and `tick_drag_autoscroll`
     grows selection by panning one visual row at an edge.
     - `crates/edit/src/lib.rs:309`
     - `crates/edit/src/lib.rs:1202`
   - Virtual documents should plug into this same routing. Do not add transcript-only
     mouse paths. Replace buffer row/byte lookups under `Window`, not the capture model.

3. **Focus and cursor ownership are separate state machines**
   - `app_focus` decides whether prompt or content owns the cursor, while `Ui::focus`
     decides overlay/keymap focus.
     - `crates/tui/src/app.rs:239`
     - `crates/tui/src/app/render_loop.rs:136`
   - Active drags can paint a cursor on the captured leaf even if that leaf is not the
     keyboard focus.
     - `crates/tui/src/app/render_loop.rs:101`
     - `crates/edit/src/lib.rs:1026`
   - Virtual transcript cursor state must preserve this split: a content cursor is a
     `DocPos`, but cursor visibility still comes from frame-level ownership.

4. **Overlay natural sizes and list-like views currently read buffers**
   - Overlay `Fit`/`Min`/`Max` sizing reads `buf.lines().len()` and longest line.
     - `crates/edit/src/lib.rs:2211`
     - `crates/edit/src/lib.rs:2229`
   - A virtual document needs a bounded `natural_size(cap)` path. It must not full
     materialize a huge transcript because an overlay asks for a fit size.
   - Picker/list can remain buffer-backed at first, but the document API should be
     suitable for replacing their eager `write_buffer` path later.

5. **Session restore, load, rewind, and block snapshots**
   - Restored sessions call `restore_screen`, finish active transcript state, then scroll
     to bottom.
     - `crates/tui/src/app.rs:1312`
     - `crates/tui/src/app/lua_handlers.rs:180`
   - Rewind uses user-turn block indices, not display rows.
     - `crates/tui/src/lua/api/session.rs:356`
     - `crates/tui/src/app/lua_handlers.rs:147`
   - `transcript_block_snapshots` currently returns `u16` row starts/counts and only has
     data after projection has run.
     - `crates/tui/src/app/transcript.rs:204`
     - `crates/tui/src/app/transcript.rs:210`
   - Virtual transcript should derive block snapshots from the node index/height index,
     use large rows, and not require an eager full projection before a rewind dialog can
     list turns or jump to a block.

6. **Theme and settings invalidation must be explicit**
   - `install_theme` and `mutate_theme` currently invalidate transcript projection.
     - `crates/tui/src/app/transcript.rs:307`
     - `crates/tui/src/app/transcript.rs:312`
     - `crates/tui/src/app/transcript.rs:319`
   - `update_settings` currently propagates Vim enablement to prompt and transcript.
     - `crates/tui/src/commands.rs:316`
   - Virtual documents need monotonic revisions for theme/style and view settings
     (`show_thinking`, width, view state). Cache keys should not depend on comparing
     whole theme values, and settings changes must preserve a document anchor.

7. **Full-materialization APIs are easy to call accidentally**
   - `UiHost::rows_for`, `breaks_for`, `smelt.transcript.text()`, and
     `smelt.transcript.blocks()` must remain exact but expensive.
     - `crates/edit/src/lib.rs:2067`
     - `crates/tui/src/app/transcript.rs:129`
     - `crates/tui/src/lua/api/transcript.rs:17`
   - Add counters around these paths and keep them out of the normal render loop. Tests
     should assert first-frame render does not call any full-materialization API.

8. **Vim/text-object semantics are byte-oriented today**
   - Vim motions and text objects operate over `&str` byte positions.
     - `crates/edit/src/vim.rs:54`
     - `crates/edit/src/motions.rs:1`
     - `crates/edit/src/text_objects.rs:1`
   - `BufferDocument` must keep this exact. `TranscriptDocument` should implement a
     document-motion subset first and deliberately materialize for unsupported text
     objects rather than silently changing behavior.

9. **Lazy height correction can move the world**
   - Estimated node heights are unavoidable for cold restored sessions unless heights
     are persisted.
   - When a node's estimate becomes exact, update prefix sums and restore view by
     `(BlockId, local_row)` anchor, not by raw absolute row. Otherwise scrollbars,
     tail-follow, and selection endpoints can jump.

## Migration phases

The phases below are intentionally larger than scaffolding-only steps. Each one should
be mergeable, improve the architecture, and leave the app shippable. Avoid building a
parallel transcript-only UI that will be thrown away later.

### Phase 0: Mechanical cleanup, tests, and instrumentation baseline

Do before architectural changes:

- Rename workspace package names as described in **Workspace crate-name cleanup** in a
  separate mechanical commit.
- Capture perf profile for a huge restored session. Baseline recorded before any
  architectural changes:

  ```text
  compositor:project_transcript    count 3    total 300.4ms  avg 100.2ms
  render:tool_call                  count 758  total 219.2ms  avg 289µs
  render:build_diff_cache           count 234  total 139.9ms  avg 598µs
  render:markdown                   count 17   total 72.3ms   avg 4.3ms
  project:render                    count 1    total 62.1ms   avg 62.1ms
  render:inline_diff_cached         count 238  total 52.7ms   avg 221µs
  render:text                       count 16   total 45.3ms   avg 2.8ms
  render:code_block                 count 35   total 36.7ms   avg 1.0ms
  render:compacted                  count 1    total 27.0ms   avg 27.0ms
  render:thinking                   count 123  total 21.4ms   avg 174µs
  render:build_file_view_cache      count 4    total 15.0ms   avg 3.8ms
  ```

  Allocation baseline:

  ```text
  compositor:project_transcript  (bytes)    count 3    total 154.65MB  avg 51.55MB
  render:build_diff_cache        (bytes)    count 234  total 80.94MB   avg 354.2KB
  render:tool_call               (bytes)    count 758  total 17.85MB   avg 24.1KB
  project:render                 (bytes)    count 1    total 9.70MB    avg 9.70MB
  render:markdown                (bytes)    count 17   total 6.03MB    avg 363.2KB
  render:code_block              (bytes)    count 35   total 4.62MB    avg 135.2KB
  render:build_file_view_cache   (bytes)    count 4    total 3.86MB    avg 987.1KB
  render:text                    (bytes)    count 16   total 3.14MB    avg 201.0KB
  render:compacted               (bytes)    count 1    total 2.89MB    avg 2.89MB
  ```

  Key observations:
  - First frame spends ~300ms in transcript projection alone.
  - 758 tool render calls happen even though only a viewport-sized subset is visible.
  - Transcript projection allocates ~155MB across 593k allocations on first frame.
  - Diff cache building is the second-largest cost at ~140ms for 234 calls.

  Target metrics to track during migration:
  - first frame total;
  - tool render count (should drop to visible/overscan only);
  - visible rows rendered;
  - full materialization calls (should be zero in normal render);
  - row cache hit/miss counts;
  - allocation bytes in transcript projection (should drop to ~viewport size).
- Add or tighten regression tests around current transcript/window behavior:
  - block layout snapshots;
  - copy/yank semantics;
  - soft vs hard line breaks;
  - resize anchoring;
  - tail-follow freeze during selection and Visual mode;
  - scrollbar drag and drag autoscroll;
  - scroll to block/tail.

Completed:

- [x] Crate rename commit (`chore: rename workspace crates to smelt-* prefix`).
- [x] Baseline perf profile recorded in PLAN.md.
- [x] Regression tests added:
  - `tail_follow_frozen_with_selection` — selection anchor freezes tail-follow.
  - `tail_follow_frozen_with_visual_mode` — Vim Visual/VisualLine freezes tail-follow.
  - `apply_tail_follow_respects_frozen` — `Ui::apply_tail_follow` skips frozen windows.
  - `scrollbar_drag_maps_thumb_to_scroll_top` — thumb position maps to correct scroll.
  - `scroll_anchor_restored_after_terminal_resize` — resize restores logical position.

Success criteria:

- No behavior changes except Cargo package names/import paths.
- Existing tests pass after the rename.
- Baseline metrics make first-frame full materialization visible.

### Phase 1: Behavior-preserving document/window foundation

One meaningful foundation commit series:

- Add `RowIndex`, `DisplayRow`, `DocPos`, document anchors, and a `Document` interface.
- Implement `BufferDocument` over existing `Buffer`/`WrappedLayout` behavior.
- Widen vertical row state from `u16` to `RowIndex` internally:
  - `Window` scroll/cursor state;
  - `WindowViewport.total_rows`;
  - scrollbars;
  - scroll links;
  - scroll events;
  - Lua `Win:scroll`, `Win:cursor`, `Win:move_cursor`;
  - picker/list helper math.
- Keep terminal sizes, screen cell coordinates, and horizontal scroll as `u16`.
- Make `Window` store cursor/selection/drag/yank-flash positions in document terms while
  preserving exact byte offsets for `BufferDocument`.

Success criteria:

- Buffer-backed windows render, scroll, select, copy, and run Vim exactly as before.
- Tests cover document/window rows beyond `u16::MAX`.
- Lua reports large `top`/`total` values without truncation.
- No transcript virtualization yet; transcript still works through the compatibility
  buffer path.

### Phase 2: Range-based rendering and explicit materialization seams

Make visible-row access the normal `Window`/`Ui` path before changing transcript:

- Change window paint to request only the visible row range plus bounded overscan from
  the backing document.
- Keep `BufferDocument` backed by existing `WrappedLayout` and buffer metadata.
- Add range-based host APIs next to full APIs:

  ```text
  rows_for_range(win, start, count)
  breaks_for_range(win, start, count)
  visible_range(win)
  ```

- Keep old full APIs implemented by explicit collection/materialization.
- Add metrics/counters for every full-materialization path.
- Update overlay natural-size plumbing so virtual documents can answer bounded
  `natural_size(cap)` without materializing all rows.

Success criteria:

- Buffer-backed render snapshots do not change.
- A test document proves `Window::render` does not request off-screen rows.
- Normal frame rendering does not call `UiHost::rows_for`/`breaks_for` full paths.

### Phase 3: Lazy `TranscriptDocument` in the normal render path

This is the first production performance win and should include visible-only tool
rendering, not a half-step that still prerenders all tools.

- Implement `TranscriptDocument` with:
  - node list from `BlockHistory`;
  - `BlockId`/layout-key identity;
  - per-node row/layout cache;
  - prefix-sum/Fenwick height index;
  - document anchors `(BlockId, local_row)`;
  - exact copy/selection/yank-flash operations over document positions.
- Replace normal `project_transcript_buffer` rendering with document row requests.
- Move Lua tool render invocation into visible/overscan node realization.
- Preserve fallback rendering for tools without a custom `render` hook.
- Keep compatibility materialization for:
  - `smelt.transcript.text()`;
  - `smelt.transcript.blocks()`;
  - `UiHost::rows_for(TRANSCRIPT_WIN)`;
  - `UiHost::breaks_for(TRANSCRIPT_WIN)`;
  - `smelt.win.transcript():buf():lines()`.

Success criteria:

- First frame after restoring a huge session renders only viewport + overscan nodes.
- No eager all-tool `prerender_tool_blocks` call in normal render.
- Restored session with hundreds of tool calls invokes only visible/overscan tool render
  hooks.
- Scrolling to an old tool renders it on demand.
- Existing transcript copy/selection/scroll/Vim-content-mode tests pass.

### Phase 4: Height persistence and jump-anywhere polish

Make cold restored sessions accurate, not merely fast:

- Persist exact node heights keyed by block identity/content/layout key/view settings.
- On restore, initialize the height index from persisted heights where valid.
- For missing heights, use estimates and correct them when rendered/measured.
- Anchor viewport by `(BlockId, local_row)` while estimates settle.
- Make block snapshots and jump-to-block use the height index directly.

Success criteria:

- Scrollbar total and jump-to-block are accurate for restored sessions with valid height
  records.
- Jumping to an unrendered block renders only target viewport synchronously.
- Estimate correction does not visibly jump tail-follow, selection, or content cursor.

### Phase 5: Public virtual APIs and second consumer

Only after transcript proves the model:

- Add virtual-friendly Lua APIs:
  - `smelt.transcript.rows(start, count)`;
  - `smelt.transcript.visible_blocks()`;
  - `smelt.transcript.block_at_row(row)`;
  - `win:visible_range()`.
- Add generic Lua document APIs if plugin authors need large custom views.
- Migrate picker/list windows to virtual documents as the second internal consumer.
- Add lazy layout leaves for large text/diff/file/custom providers where they pay off.

Success criteria:

- Plugin authors can build large views without eager buffers.
- Existing `Buf`/`Win` APIs still work for buffer-backed plugins.
- Picker/list migration validates that the abstraction is generic, not transcript-only.

## Workspace crate-name cleanup

The workspace currently mixes prefixed and unprefixed package names while folders are
already short and should remain short. The cleanup should rename Cargo package names
and dependency keys, not crate folders.

Current package names:

```text
crates/protocol       protocol
crates/engine         engine
crates/style          smelt-style
crates/perf           smelt-perf
crates/buffer         smelt-buffer
crates/term           smelt-term
crates/edit           smelt-edit
crates/core           smelt-core
crates/tui            tui
crates/lua-doc-derive lua-doc-derive
crates/xtask          xtask
root package          smelt-agent
```

Recommended package names:

```text
protocol        -> smelt-protocol
engine          -> smelt-engine
tui             -> smelt-tui
lua-doc-derive  -> smelt-lua-doc-derive
```

Keep these exceptions:

```text
xtask           stays xtask
root package    stays smelt-agent
```

Decision:

- `lua-doc-derive` is a proc-macro helper, but it is still an internal workspace
  crate used by production crates at compile time. Prefix it too for consistency.

Implementation checklist:

1. Update each affected crate's `[package].name` in its `Cargo.toml`.
2. Update `[workspace.dependencies]` in the root `Cargo.toml`:

   ```toml
   smelt-protocol = { path = "crates/protocol" }
   smelt-engine = { path = "crates/engine" }
   smelt-tui = { path = "crates/tui", default-features = false }
   smelt-lua-doc-derive = { path = "crates/lua-doc-derive" }
   ```

3. Update every crate dependency key from unprefixed to prefixed.
4. Update Rust import paths where the library crate name changes:
   - package `smelt-protocol` imports as `smelt_protocol` unless `[lib] name =
     "protocol"` is kept;
   - package `smelt-engine` imports as `smelt_engine` unless `[lib] name =
     "engine"` is kept;
   - package `smelt-tui` imports as `smelt_tui` unless `[lib] name = "tui"` is
     kept.
5. Prefer changing Rust crate names too for consistency, not just Cargo package names,
   unless a large external API break is undesirable.
6. Update tests, examples, fuzz targets, xtask dependencies, and docs that refer to old
   package names.
7. Run:

   ```bash
   cargo metadata --no-deps
   cargo fmt
   cargo nextest run --workspace
   cargo clippy --workspace --all-targets -- -D warnings
   ```

Suggested sequencing:

- Do this cleanup as a separate mechanical commit before virtual-document work.
- It will reduce ambiguity in future architecture changes and avoid mixing package
  rename diffs with behavior changes.

## Performance budget

Target for a huge restored session first frame:

- `render:tool_call` count proportional to visible/overscan tool blocks, not session
  total.
- transcript row stitching allocation proportional to viewport rows, not full rows.
- first-frame `compositor:project_transcript` should be bounded by visible nodes and
  height-index work.
- full transcript materialization should show up only when compatibility APIs are
  explicitly called.

Suggested metrics:

```text
transcript:virtual_total_rows
transcript:visible_nodes
transcript:node_render_miss
transcript:node_render_hit
transcript:height_estimate_used
transcript:height_corrected
transcript:full_materialize
render:tool_call_visible
```

## Risk areas

1. **Selection and Vim byte offsets**
   - Highest regression risk. Keep `BufferDocument` byte semantics exact and introduce
     transcript `DocPos` carefully.
2. **Lua compatibility**
   - Existing APIs such as `Buf:lines()` and `smelt.transcript.text()` must remain
     exact, even if expensive.
3. **Scroll/cursor row widening**
   - Many `u16` clamps are scattered through UI, Lua, picker, and events.
4. **Gutters**
   - Existing gutter providers are `Buffer`-centric.
5. **Theme/style cache invalidation**
   - Anonymous style groups and theme resolution must not produce stale colors.
6. **Tool render side effects**
   - Lua render hooks may have implicit side effects today because they run eagerly.
     Moving them to visible-only changes timing. Audit bundled plugins and document
     that tool render callbacks must be pure render functions.

## Validation checklist before merging each phase

Run at minimum:

```bash
cargo fmt
cargo nextest run --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Focused manual/regression scenarios:

- Restore huge session and verify first frame is tail-visible quickly.
- Scroll up/down with wheel and keyboard.
- Drag scrollbar on huge transcript.
- `win:scroll()` get/set/tail from Lua.
- Tail-follow while streaming.
- Tail-follow freezes during selection and Visual mode.
- Resize terminal while scrolled away from tail.
- Jump to prior transcript block / rewind target.
- Copy/yank transcript ranges spanning:
  - markdown table rows;
  - tool output;
  - soft-wrapped lines;
  - rows with non-selectable chrome;
  - rows with `copy_as` or `source_text`.
- Double-click word/table cell and triple-click row/block.
- Vim visual selection and yank in transcript.
- Search-like consumers that call `UiHost::rows_for` or transcript text.
- `smelt.transcript.text()` and `smelt.transcript.blocks()` output parity.
- Tool render hooks for visible old tools and off-screen old tools.
- Theme change while scrolled into old transcript.
- Toggle `show_thinking`.
- Picker/list windows and overlay list navigation.
- Lua paint leaves and per-window renderers.

## Implementation recommendation

Do this in phases and keep each phase mergeable. The first meaningful production win
comes when transcript normal rendering uses `TranscriptDocument` visible rows and tool
render hooks move to visible-only execution. The abstraction work before that is
necessary to avoid a transcript-specific fork of scrolling, selection, and rendering
logic.

The key design principle is:

> `Window` owns viewport behavior; `Document` owns row production and document
> positions. Transcript is one document provider, not a special window.
