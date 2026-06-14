# Transcript Layout Rewrite — Architectural Plan

**Goal:** make transcript projection (resume, preview, resize, first render) scale from 10 MB to 100 MB sessions while keeping exact scrollbar, exact vim navigation, and real rendering semantics. End state: simpler, more composable, more robust code with the right abstractions and no leftover scaffolding.

**Planning principles:**

1. This plan is a direction, not a contract. If a better approach emerges during implementation, we take it.
2. We do not defer work just because it is large. If something is worth doing for the end-state quality of the code, we do it now.
3. Better/simpler code is the goal. The final code must be simpler, less complex, more composable, more robust, and more testable than what we have today. Whatever structural changes are required to achieve that are acceptable.
4. APIs can break and evolve. We do not keep old APIs, internal shims, backward compatibility layers, or Lua surface compatibility for their own sake.
5. Remove before adding. If an abstraction no longer pulls its weight, delete it rather than wrap it.
6. Refactor/consolidate along the way. If cleanup is worth doing for the final architecture, do it as part of the migration instead of deferring it because it is large.
7. Exactness is non-negotiable. Speed must come from avoiding wasted work, not from faking data.
8. Implementation code, comments, test names, and commit messages should describe the enduring domain behavior, not migration bookkeeping. Phase labels belong in this plan and task notes, not in production identifiers or committed test names.
9. Keep this plan current as work lands: mark completed slices with code references, baseline numbers, and any changed conclusions so later phases start from the current truth rather than stale intent.

---

## 1. What we found

### 1.1 The concrete symptom

An 11 MB session causes ~2 s stalls on:

- `/resume` picker open (initial preview)
- selecting/resuming the session
- terminal resize

Benchmark highlights:

- `render:build_diff_cache` ×1024 → 1.27 s
- `render:inline_diff_cached` ×1024 → 331 ms
- `render:tool_call` ×1024 → 277 ms
- `render:markdown` / `render:text` ×670 → ~170 ms each
- `compositor:render_flush` max → 1.57 s
- `cmd:dispatch` / `lua:cmd` / `lua:event_cb` max → 1.81 s

The 1024 counts are suspicious: for a large transcript, almost every block is being rendered even though only a viewport-full is visible.

### 1.2 Root cause

The system already virtualizes *viewport materialization*, but it pays for a full *rendered measurement pass* first.

Current pipeline:

```
history
  → TranscriptProjection
    → ExactRowIndex wants heights
      → RenderedBlockCache renders every block into a Buffer
        → count Buffer lines
      → prefix rows
    → plan visible block range
    → render/materialize visible blocks
```

`measure_all_heights` (`crates/tui/src/content/transcript_buf.rs:439`) is the seam where the problem lives. To know how tall a block is, the code currently lays it out into a real `Buffer` with spans, highlights, syntect, diff caches, and tool-render callbacks — then counts the lines.

Consequences:

1. **Resize is catastrophic** — width change invalidates `RenderedBlockCache` and `ExactRowIndex`, so every block must be re-rendered.
2. **Resume preview is synchronous and full** — the resume dialog calls `render_preview_into` which builds a full `TranscriptView`, then prerenders *all* tool blocks, then measures all heights.
3. **Tool prerender is global** — every compositor frame calls `prerender_transcript_tool_blocks_for_ids(tw, &block_ids)` for the *entire* transcript order (`crates/tui/src/app/render_loop.rs:117-119`). On resize every historical tool misses its width-keyed cache and reruns its Lua render hook.
4. **Markdown has no AST/IR** — it parses and emits in one pass; measuring height has no cheaper path.
5. **Diffs are re-cached eagerly** — `extract_rendered_layout` builds `CachedInlineDiff` during tool extraction, under a width-keyed cache, so resize rebuilds them all.

### 1.3 What already exists that is good

- `BlockHistory` — stable transcript block model.
- `LayoutKey` — clean cache key: width + show_thinking + view_state + content_hash + sidecar_hash.
- `ExactRowIndex` — prefix-sum row index; keep it, just feed it from cheaper measurement.
- `BlockLayout` / `RenderedLayout` — declarative layout tree for tool output. Keep the idea, evolve the leaves.
- `CachedInlineDiff` — a real width-independent-ish IR for diffs: diff lines + syntax style ranges; wrap/render from the IR.
- Visible-row materialization in `project_visible_range` / `collect_blocks_range`.

### 1.4 What should go away or change

- `RenderedBlockCache` as the source of truth for heights. It stores fully rendered `Buffer`s and is cleared on every width change. It can remain as a *visible-row rendering cache* but should not be required for measurement.
- The global tool-prerender pass every frame.
- The assumption that tool render hooks must run for every historical tool on resize.
- Markdown rendering doing parse+emit in one pass with no reusable IR. Replace the parser with `pulldown-cmark`-powered AST; keep Smelt's custom rendering decisions (heading spacing, table layout, etc.).
- Diff cache being built inside width-keyed tool render cache.
- `build_rows` / `display_rows_for_range` / `copy_range` all forcing full `measure_all_heights` through rendered buffers.
- `ToolState.layout_revision` as the sidecar cache key. It is in-memory and volatile; replace it with a stable content hash of the mutable tool state (status, output, user_message, elapsed).
- `layout.leaf(buf_id)` from the tool renderer API. It forces width-dependent rendered buffers and blocks width-independent IR. Remove it; the API becomes declarative primitives (`layout.text`, `layout.diff`, `layout.file_view`, compositional `vbox`/`hbox`).

---

## 2. Goal architecture

### 2.1 Core idea

Separate **layout** (width-dependent height) from **rendering** (painting visible rows).

```
Session history
  → BlockHistory
    → hydrate DisplayIR from persistent cache
    → compile cache misses to width-independent DisplayIR
      → DisplayIR is cheap to measure at any width
      → DisplayIR can render an arbitrary row range at any width
  → ExactRowIndex
    → heights from DisplayIR.measure(width)
    → prefix rows
  → ProjectionPlan
    → visible block range
  → Viewport materialization
    → render only visible row ranges from DisplayIR
```

Measurement must not:

- allocate terminal buffers,
- run syntect,
- build diff caches,
- call Lua tool render hooks,
- emit styled spans.

Rendering must:

- reuse the same DisplayIR so semantics match measurement exactly,
- only touch visible rows,
- still support copy/yank/selection/search by being able to render any row range on demand.

### 2.2 Desired properties

| Property | Today | Goal |
|----------|-------|------|
| Exact scrollbar total | yes, via full render | yes, via IR measurement |
| Exact `gg`/`G`/scroll | yes, via full render | yes, via prefix index |
| Resize latency | O(all blocks × full render) | O(all blocks × cheap measure) |
| Resume latency | full render + tool rerender | hydrate persistent IR cache, cheap measure, render visible |
| Preview latency | synchronous full render | same as live transcript |
| 100 MB feasibility | no | yes, if IR stays proportional to source |
| Code complexity | several overlapping caches | one IR, one row index, one visible renderer |

### 2.3 Design principles

1. **Plan is direction, not contract.** We adapt as we learn.
2. **No deferred big work.** If something is worth doing for end-state quality, we do it now.
3. **Better code is the goal.** Simpler, less complex, more composable, more robust, more testable.
4. **APIs can break and evolve.** We do not keep old APIs, backward compatibility layers, internal shims, or Lua surface compatibility for their own sake. If a cleaner design requires changing tool renderer contracts, session serialization, or module boundaries, we change them.
5. **One canonical source state.** `Session` / `BlockHistory` remain the canonical transcript state. DisplayIR is the canonical display form derived from that state, not a replacement for it.
6. **DisplayIR is serializable from day one.** `session.ir.bin` can land after the first useful vertical slice, but every DisplayIR type must be designed as cache-safe: width-independent, theme-independent, deterministic, and free of Lua handles, `Buffer`s, render caches, and non-serializable state.
7. **Width only affects wrapping, not parsing.** Compile the IR once per block content, not per width.
8. **Rendering is a function `IR × width × row_range → rows`.** No hidden global state needed to paint a row.
9. **Keep `ExactRowIndex` but change how it is fed.** It is a good abstraction; the bad part is rendered measurement.
10. **Tool render hooks run only for content changes/cache misses.** Width changes and hidden historical blocks with valid DisplayIR do not call Lua.
11. **Remove before adding.** If an abstraction no longer pulls its weight, delete it rather than wrap it.
12. **Refactor/consolidate during the migration.** No deferred big cleanup just because it is large. If finishing a refactor is worth it for the end state, finish it in this migration.
13. **No approximate previews.** Exactness is non-negotiable; the speed must come from avoiding wasted work, not from faking data.

---

## 3. Proposed data model

### 3.1 Canonical state vs display cache

`Session` / `BlockHistory` remain the canonical source state. Blocks keep their source strings and structured domain data:

```rust
Block::Text { content: String }
Block::Thinking { content: String }
Block::ToolCall { call_id, name, summary, args }
```

DisplayIR does **not** live inside `Block`. It lives in `DisplayModel` and in the disposable `sessions/<id>/session.ir.bin` sidecar. This keeps conversation/session state clean and makes the display cache safe to delete and rebuild.

Every DisplayIR type must be serializable/cache-safe from the beginning, even before the persistent cache implementation lands. Do not put theme-dependent syntax data, Lua userdata, `Buffer`s, `OnceCell` render caches, or width-dependent wraps inside DisplayIR.

### 3.2 `DisplayBlock` / `DisplayIr` (new, central)

Owned by `Transcript` or a parallel `DisplayModel` keyed by display-cache identity, not by bare `BlockId` alone. `BlockId`s are stable across rewrites, so reuse must validate the current content and sidecar hashes.

```rust
pub(crate) struct DisplayCacheKey {
    block_id: BlockId,
    content_hash: u64,
    sidecar_hash: u64,
    renderer_version: u32,
}
```

```rust
/// Width-independent display representation of one transcript block.
pub(crate) enum DisplayBlock {
    Markdown(MarkdownBlock),
    Code(CodeBlock),
    Tool(ToolBlock),
    User(UserBlock),
    Thinking(MarkdownBlock),
    Compacted(MarkdownBlock),
    Mode(ModeBlock),
    ProcessStatus(ProcessStatusBlock),
    Exec(ExecBlock),
}
```

Each variant holds only semantic data and precomputed cheap-to-store metrics:

- For text: run-aware `InlineLine`s with style/source/copy metadata; widths are computed on demand.
- For code: lines + tab expansion state; syntax tokens are computed only by ephemeral render caches.
- For tools: the tool header + a `ToolBodyIr` (see below).
- For diffs: a `DiffIr` similar to `CachedInlineDiff` but without render state.

Persistent DisplayIR is stored separately from the canonical session file. The session JSON remains the source of truth; the display cache is disposable and keyed by content/sidecar/renderer versions. Theme-dependent syntax, visible rows, and width-dependent wraps never go into the persistent cache.

### 3.3 `MarkdownBlock`

Use `pulldown-cmark` **only as the parser** and translate its event stream into a small, purpose-built AST. Smelt keeps its own rendering logic: heading spacing, list bullets, blockquote bars, code block chrome, and especially table layout (dynamic column fitting, stacked fallback, borders, alignment, and soft-wrapping inside cells) all remain unchanged. The only thing that changes is where the structural tree comes from.

Build the AST from `pulldown-cmark::Parser::into_offset_iter()` so block nodes and inline spans can carry source byte ranges into the original markdown. The semantic AST is not enough on its own: copy/yank fidelity depends on attaching original markdown source to rendered rows.

This replaces the custom line-oriented markdown renderer in `display_renderers/markdown.rs` and fixes bugs that are fundamentally parsing bugs: malformed/nested code blocks, tables whose inline code spans were not recognized as a single cell token, escaped characters, and other cases where the hand-rolled parser lost nesting information. Rendering was not the problem; the AST was.

Dependency:

```toml
pulldown-cmark = { version = "0.13", default-features = false }
```

With default features disabled the transitive deps are only `bitflags`, `memchr`, `unicase`, and `unicode-width`.

```rust
pub(crate) struct MarkdownBlock {
    source: String,
    nodes: Vec<MarkdownNode>,
}

pub(crate) struct SourceRange {
    start: usize,
    end: usize,
}

pub(crate) enum MarkdownNode {
    Paragraph { spans: Vec<InlineSpan>, source: SourceRange },
    Heading { level: u8, spans: Vec<InlineSpan>, source: SourceRange },
    BlockQuote { children: Vec<MarkdownNode>, source: SourceRange },
    List { ordered: bool, items: Vec<Vec<MarkdownNode>>, source: SourceRange },
    /// A fenced or indented code block. The AST stores raw source lines;
    /// rendering applies chrome, tab expansion, and optional syntax highlighting.
    Code { lang: String, lines: Vec<String>, source: SourceRange },
    Table(TableIr),
    Rule { source: SourceRange },
    Blank,
}

pub(crate) enum InlineSpan {
    Text { text: String, source: SourceRange },
    Bold { children: Vec<InlineSpan>, source: SourceRange },
    Italic { children: Vec<InlineSpan>, source: SourceRange },
    Code { text: String, source: SourceRange },
    Strikethrough { children: Vec<InlineSpan>, source: SourceRange },
    Link { text: Vec<InlineSpan>, url: String, source: SourceRange },
    Image { alt: String, url: String, source: SourceRange },
    TaskListMarker { checked: bool, source: SourceRange },
}
```

`InlineSpan` is a tree, not a flat run list, because `pulldown-cmark` naturally produces nested emphasis/links. Measurement flattens spans to compute display width and wrap count; rendering flattens spans into styled text. We do not store cumulative widths in the IR; wrapping is computed on demand during measure/render.

Parser options enabled:

- `ENABLE_TABLES`
- `ENABLE_STRIKETHROUGH`
- `ENABLE_TASKLISTS`
- optionally `ENABLE_HEADING_ATTRIBUTES` and `ENABLE_SMART_PUNCTUATION`

Rendering choices we deliberately keep:

- No blank line between a heading and the following content.
- Custom table measurement/rendering: `pulldown-cmark` only gives rows; column widths, wrapping, fallback, and borders stay in Smelt.
- `source_text` / copy-yank semantics must continue to return the original markdown, even for rendered tables and chrome-delimited blocks.
- Table and chrome rows preserve non-selectable spans, `cell_selectable`, `block_selectable`, `copy_continuation`, and `pre_formatted` metadata.

Unsupported markdown policy:

- Support table alignment, escaped characters, links, images, task lists, hard/soft breaks, emphasis/strong/strikethrough, inline code, fenced/indented code, blockquotes, lists, headings, horizontal rules, and tables.
- Inline HTML, block HTML, footnotes, metadata/frontmatter, and unusual extensions initially render as plain text where possible.
- Never silently drop user-visible text. Unsupported structure degrades to plain text plus source ranges.

### 3.4 `CodeBlock`

```rust
pub(crate) struct CodeBlock {
    lang: String,
    lines: Vec<InlineLine>,
}
```

Height does not need syntax. `CodeBlock` IR is pure semantic/layout data: source lines, language, and cheap text metrics. Syntax highlighting is not stored in the IR and is not serialized with it; it lives in a separate render cache owned by the projection/renderer and is computed only for visible rows.

### 3.5 `ToolBlock`

```rust
pub(crate) struct ToolBlock {
    name: String,
    summary: StyledLines,
    status: ToolStatus,
    elapsed: Option<Duration>,
    user_message: Option<String>,
    body: ToolBody,
}

pub(crate) enum ToolBody {
    /// Default wrapped/plain output.
    Text(Vec<InlineLine>),
    /// Rich layout produced by a tool renderer.
    Layout(LayoutIr),
    /// Denied / no output.
    Empty,
}
```

### 3.6 `LayoutIr` (evolution of `BlockLayout`/`RenderedLayout`)

Replace the current `BlockLayout<BufId>` / `BlockLayout<Box<Buffer>>` pair with a single renderable/measurable IR.

```rust
pub(crate) enum LayoutIr {
    /// Plain text runs, cheap to measure and render.
    Text(Vec<InlineRun>),
    /// Precomputed diff, width-independent.
    Diff(DiffIr),
    /// File view, width-independent.
    FileView(FileViewIr),
    /// Horizontal stack of children with width constraints.
    Hbox(Vec<HboxItem>),
    /// Vertical stack of children.
    Vbox(Vec<LayoutIr>),
    /// Solid separator line.
    Separator { glyph: char },
}

pub(crate) struct HboxItem {
    constraint: Constraint,
    child: LayoutIr,
}
```

Leaves no longer carry rendered `Buffer`s. `measure(width)` recursively computes heights. `render(width, row_range, sink)` recursively paints visible rows.

The primary API has no buffer-leaf compatibility path. `layout.leaf(buf_id)` is removed. If preformatted content becomes necessary for a concrete built-in use case, add it as a new declarative primitive with explicit semantics; do not keep the old buffer-leaf model alive.

### 3.7 `DiffIr` / `FileViewIr`

Evolve `CachedInlineDiff` into `DiffIr`.

```rust
pub(crate) struct DiffIr {
    lines: Vec<DiffLine>,
    max_lineno: u32,
}

pub(crate) enum DiffLine {
    Context { lineno: u32, text: InlineLine },
    Insert { lineno: u32, text: InlineLine },
    Delete { lineno: u32, text: InlineLine },
    Ellipsis,
}
```

The IR stores the diff structure and line metrics. Syntax highlighting is a render concern and is cached outside the IR. Wrapping is computed at measure/render time from `InlineLine`.

### 3.8 `InlineLine` (shared wrapping primitive)

Wrapping must understand styled runs, source ranges, copy substitutions, and unbreakable inline spans. A plain string metric is not enough for markdown paragraphs, table cells, links, and inline code.

```rust
pub(crate) struct InlineLine {
    runs: Vec<InlineRun>,
}

pub(crate) struct InlineRun {
    text: String,
    style: InlineStyle,
    source: Option<SourceRange>,
    break_policy: BreakPolicy,
    copy_as: Option<String>,
}

pub(crate) enum BreakPolicy {
    Normal,
    BreakOnSpaces,
    Unbreakable,
    PreserveSpaces,
}

impl InlineLine {
    fn plain(s: String) -> Self;
    fn measure_unwrapped(&self) -> CellWidth;
    fn wrap_rows(&self, max_cells: u16) -> RowIndex;
    fn wrap_ranges(&self, max_cells: u16) -> Vec<WrappedInlineLine>;
}
```

Plain text, code, diff lines, tool output, ANSI output, and markdown inline content all lower to `InlineLine`; plain text is a single run. Do not precompute cumulative char widths initially. Width walking is computed on demand; add a lazy width cache only if profiling shows it matters.

---

## 4. Proposed API surfaces

### 4.1 `TranscriptProjection` becomes the layout engine

```rust
pub(crate) struct TranscriptProjection {
    /// Width-independent IR per block/cache key.
    display_blocks: HashMap<DisplayCacheKey, DisplayBlock>,
    /// Exact row index: prefix rows for current (width, show_thinking, view_state).
    exact_rows: ExactRowIndex,
    /// Last materialized visible range.
    visible_layout: Vec<LayoutEntry>,
    /// Ephemeral render-only caches: syntax highlighting, recently rendered visible rows, etc.
    render_caches: RenderCaches,
}
```

Key operations:

```rust
impl TranscriptProjection {
    /// Compile or reuse DisplayBlock for each block. Cheap, does not depend on width.
    fn ensure_display_blocks(&mut self, history: &BlockHistory);

    /// Recompute exact row index for a width. O(blocks × cheap measure).
    fn rebuild_row_index(&mut self, history: &BlockHistory, width: u16, show_thinking: bool);

    /// Plan visible range.
    fn plan_projection_measured(...);

    /// Materialize only planned visible rows.
    fn project_planned(&mut self, buf: &mut Buffer, theme: &Theme, plan: ProjectionPlan);
}
```

### 4.2 `DisplayBlock` trait/object

```rust
impl DisplayBlock {
    fn measure(&self, ctx: MeasureCtx) -> BlockMeasurement;
    fn render_range(&self, ctx: RenderCtx, rows: Range<RowIndex>, out: &mut RowSink);
}
```

`MeasureCtx` carries width, show_thinking, view_state, and theme-independent layout constants.

`RenderCtx` carries width, theme, row budget, and render-cache access.

View-state handling is a wrapper around block IR measurement/rendering. `DisplayBlock.measure(ctx)` returns post-view-state height, and `render_range` receives row ranges in post-view-state coordinates. Collapsed/trimmed head/trimmed tail behavior is implemented once, not duplicated in every block renderer.

Rendering emits full rows, not just strings:

```rust
pub(crate) struct RenderedRow {
    text: String,
    highlights: Vec<Span>,
    decoration: LineDecoration,
}

pub(crate) trait RowSink {
    fn push_row(&mut self, row: RenderedRow);
}
```

This preserves the current `LineBuilder` semantics without requiring a `Buffer` for measurement. Row decorations include `source_text`, `soft_wrapped`, `copy_continuation`, `cell_selectable`, `block_selectable`, `source_line`, `fill_bg`, and `pre_formatted`.

### 4.3 Keep `ExactRowIndex`

The prefix-sum index is a good abstraction. Only its feeding mechanism changes. Nodes still store:

- `BlockId`
- `LayoutKey`
- `exact_height`
- `estimated_height` (fallback)

Rebuild rules stay the same except measurement becomes cheap.

### 4.4 Rendered row cache

A small optional cache of recently materialized visible block rows. Unlike today's `RenderedBlockCache`, it stores `RenderedRow`s keyed by `(DisplayCacheKey, width, view_state, row_range)`. This is an optimization, not required for correctness.

Persistent DisplayIR and ephemeral render caches are separate. Syntax highlighting, theme-dependent spans, and visible rows are render caches; they are not part of the serialized DisplayIR.

### 4.5 Chrome metrics

Measurement is theme-independent, but it still needs every fixed cell-width layout decision used by rendering: transcript indentation, code/table chrome, diff gutters, box inner width, hbox constraints, tool header chrome, and block gaps.

```rust
pub(crate) struct MeasureCtx {
    width: u16,
    show_thinking: bool,
    view_state: ViewState,
    chrome: ChromeMetrics,
}

pub(crate) struct ChromeMetrics {
    transcript_indent: u16,
    code_border_width: u16,
    table_border_width: u16,
    diff_gutter_width: u16,
    tool_header_width: u16,
}
```

The exact fields should match the implementation, but the rule is fixed: measurement and rendering share one source for chrome widths.

---

## 5. Tool renderer API changes

### 5.1 Current situation

Tool renderers return width-dependent `BlockLayout<BufId>`, which contains buffer ids, diff specs, and file-view specs:

```lua
-- plugin returns a layout tree with Buf/Diff/FileView leaves
```

`extract_rendered_layout` converts `BufId` to `Box<Buffer>` and eagerly builds `DiffCache` from `DiffSpec`.

### 5.2 Goal

Tool renderers return `LayoutIr` directly. Lua describes what the tool output is; Rust owns measurement, wrapping, diff rendering, syntax highlighting, copy metadata, and visible-row rendering.

The primary tool render context has no width:

```rust
pub struct ToolIrCtx<'a> {
    pub summary: &'a str,
    pub status: &'a str,
    pub elapsed_secs: Option<u64>,
    pub call_id: Option<&'a str>,
}
```

`ToolRenderCtx.width` is removed from the primary API. Width may only appear if we add a new explicitly preformatted declarative primitive with fixed semantics; it is not part of normal tool layouts.

Chosen design:

- Tool renderers return a declarative tree of `{ kind = "text"|"diff"|"file"|"vbox"|"hbox"|"separator", ... }`.
- Built-in tools and bundled plugins are migrated to declarative output.
- `ctx.width` is removed from the primary tool-renderer API. Tool renderers choose structure; Rust owns width-dependent measurement and rendering.
- `layout.leaf(buf_id)` is removed from the Lua API entirely; the old buffer-leaf contract is gone.
- No compatibility shim is planned. If an explicit preformatted primitive becomes necessary for a concrete built-in use case, it must be justified as a new declarative primitive, not as preservation of the old buffer API.

### 5.3 Tool render cache changes

Remove `ToolState.render_cache` as a width-keyed rendered buffer cache. Instead, cache the `LayoutIr` (or the full `ToolBody`) inside `ToolBlock` IR.

`ToolBlock` IR should be invalidated only when:

- tool output changes,
- tool args/status change,
- tool renderer implementation/version changes.

Width changes do **not** invalidate it.

---

## 6. Migration phases

Phases are big and well-scoped, but implementation should prefer thin vertical slices over long waterfall work. After the primitives exist, convert one simple block path end-to-end through `DisplayBlock.measure` / `render_range` / `ExactRowIndex` / `copy_range` before converting every block type. This exposes hidden rendered-buffer assumptions early.

Each phase should leave the codebase in a working, testable state. `session.ir.bin` implementation lands once enough DisplayIR exists to be useful, but serializability/cache-safety is a constraint from Phase 1 onward.

Do not carry compatibility scaffolding across phases. When a new path is proven, delete the old path and consolidate call sites before moving on. If a refactor is worth doing for the final architecture, finish it in this migration rather than leaving a shim or duplicate abstraction for later.

### Phase 0: Instrumentation and validation harness

**Goal:** know exactly where time goes and have tests that prevent regressions.

- Add perf scopes around:
  - `session:load` sub-parts (read, parse, history conversion, blob internalize, DisplayIR cache read/hydrate)
  - `transcript:build_from_session`
  - `transcript:measure_all_heights` and its substeps
  - tool prerender count/time
  - diff cache build count/time
  - markdown render count/time
- Add a benchmark or test that builds a large synthetic transcript and measures:
  - first projection time,
  - resize time,
  - visible-row materialization time,
  - memory allocated during projection.
- Add property tests: for random blocks and widths, `measure(width)` must equal the number of rows produced by `render_range(width, all_rows)`.

**Deliverable:** benchmark + baselines.

**Completed slice:** instrumentation and a large mixed-transcript baseline are in place.

- Perf scopes added around session load subparts (`crates/core/src/session.rs`), session preview (`crates/tui/src/lua/api/session.rs`), session-to-transcript conversion (`crates/tui/src/app/history.rs`), tool prerender/diff extraction (`crates/tui/src/app/transcript.rs`), rendered-block cache misses (`crates/tui/src/content/block_buffers.rs`), and transcript measurement/projection/range/copy paths (`crates/tui/src/content/transcript_buf.rs`).
- The ignored baseline test is `mixed_large_transcript_projection_baseline` in `crates/tui/src/content/transcript_buf.rs`. It builds a ~10 MB mixed transcript with user text, markdown headings/tables/fenced code, thinking, exec output, and `edit_file`-style tool calls with prebuilt diff render caches. It prints one stable `TRANSCRIPT_LAYOUT_BASELINE ...` line for copying into this plan.
- Current regression coverage includes current projection range/full-row equivalence for randomized block mixes and widths. This is intentionally not the final independent `measure(width) == render_range(width, all_rows)` property; that property lands once the new IR has a true non-rendering measurement path.

Baseline command:

```bash
cargo test -p smelt-tui mixed_large_transcript_projection_baseline -- --ignored --nocapture
```

Baseline result from this branch/test build:

```text
TRANSCRIPT_LAYOUT_BASELINE input_bytes=10497943 generated_bytes=10499021 blocks=3404 total_rows=120137 diff_caches=128 diff_cache_ms=1631 resize_diff_caches=128 resize_diff_cache_ms=1626 first_ms=810 resize_ms=914 visible_ms=3 allocs=3166449 bytes_allocated=576051070 visible_rows=80
```

Top duration totals from the same run:

| label | count | total |
|---|---:|---:|
| `render:text` | 1024 | 3.461 s |
| `render:markdown` | 1024 | 3.456 s |
| `render:build_diff_cache` | 256 | 3.247 s |
| `transcript:plan_projection_measured` | 2 | 1.723 s |
| `transcript:measure_all_heights` | 3 | 1.723 s |
| `transcript:measure_all_heights:measure_chunk` | 14 | 1.711 s |
| `transcript:render_block_cache:ensure_many` | 17 | 1.708 s |
| `render:tool_call` | 256 | 1.653 s |
| `render:inline_diff_cached` | 256 | 1.645 s |
| `transcript:render_block_cache:layout_misses` | 15 | 1.588 s |
| `render:code_block` | 1024 | 875 ms |
| `render:exec` | 400 | 379 ms |
| `render:wrapped_output` | 400 | 364 ms |

Current conclusion: visible materialization after measurement is cheap (~3 ms for 80 rows). The expensive work is still eager full-transcript height measurement, which currently renders all blocks and rebuilds/render-replays diff/tool/markdown/code paths. This confirms the migration should attack width-independent IR and measurement before optimizing visible-row copying.

### Phase 1: Introduce `InlineLine` and share wrapping

**Goal:** centralize run-aware text wrapping so it can be reused by every block type.

- Move wrapping logic from markdown inline spans, code block rendering, diff printing, and tool output into one `InlineLine` / `InlineRun` primitive in `smelt_core::content::measure` or equivalent.
- `InlineLine` stores runs with:
  - display text,
  - style/copy metadata,
  - optional source ranges,
  - break policy (`Normal`, `BreakOnSpaces`, `Unbreakable`, `PreserveSpaces`).
- Do **not** precompute cumulative char widths initially. Compute width on demand and add a lazy width cache only if profiling shows it matters.
- Provide:
  - `measure_unwrapped() -> CellWidth`
  - `wrap_rows(max_cells) -> RowIndex`
  - `wrap_ranges(max_cells) -> Vec<WrappedInlineLine>`
  - `plain(s: String) -> InlineLine`
- Update markdown inline wrapping, table cell wrapping, code block wrapping, diff wrapping, ANSI/plain tool output, and file-view output to use `InlineLine`.

**Why first:** wrapping is the common primitive. Getting it right and fast unlocks everything else.

**Deliverable:** all wrapped text goes through `InlineLine`; no behavior change except where existing tests intentionally capture known bugs for later correction.

**Completed slice:** shared inline wrapping is now the wrapping primitive for transcript/content renderers.

- Added `smelt_buffer::inline_line::{InlineLine, InlineRun, WrappedRun, BreakPolicy}` in `crates/buffer/src/inline_line.rs`, re-exported as `smelt_core::content::inline_line`. It includes width measurement, run-preserving visual row wrapping, run-index/byte-range fragments for styled callers, and explicit `Normal` versus `BreakOnSpaces` policies.
- Markdown inline/table wrapping now lowers `InlineSpan`s to `InlineLine<InlineStyle>` in `crates/core/src/content/highlight/inline.rs`.
- Code block and file-view syntax wrapping now lower syntect regions to `InlineLine<Style>` with preserved-space wrapping in `crates/core/src/content/highlight/syntax.rs`.
- Inline diff rendering now lowers cached render spans to `InlineLine<(u8, u8, u8)>` with preserved-space wrapping in `crates/core/src/content/highlight/diff.rs`.
- ANSI/tool output wrapping now uses `InlineLine` break-on-space semantics in `crates/core/src/content/ansi.rs` and run-index fragments in `crates/tui/src/content/display_renderers/tools.rs`, removing the tool-title offset side table.
- User/exec chrome, process-status rows, and collapsed thinking summaries now use `InlineLine` plain byte ranges in `crates/tui/src/content/display_renderers/chrome.rs`, `process_status.rs`, and `thinking.rs`.
- Remaining direct uses of `smelt_buffer::wrap` are outside transcript/content rendering: low-level buffer visual layout (`crates/buffer/src/wrap.rs`, `crates/buffer/src/wrap_layout.rs`), the edit re-export (`crates/edit/src/text.rs`), and standalone formatting (`crates/tui/src/format.rs`). `InlineLine::wrap_plain_ranges` delegates to the low-level wrapper for plain single-run compatibility rather than duplicating that algorithm.

Validation:

```bash
cargo test -p smelt-buffer
cargo test -p smelt-core
cargo test -p smelt-tui
cargo fmt
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p smelt-tui mixed_large_transcript_projection_baseline -- --ignored --nocapture
```

Baseline result after the shared wrapping slice:

```text
TRANSCRIPT_LAYOUT_BASELINE input_bytes=10497943 generated_bytes=10499021 blocks=3404 total_rows=120137 diff_caches=128 diff_cache_ms=1652 resize_diff_caches=128 resize_diff_cache_ms=1632 first_ms=852 resize_ms=940 visible_ms=3 allocs=3359698 bytes_allocated=603078742 visible_rows=80
```

Top duration totals from the same run:

| label | count | total |
|---|---:|---:|
| `render:text` | 1024 | 3.580 s |
| `render:markdown` | 1024 | 3.576 s |
| `render:build_diff_cache` | 256 | 3.273 s |
| `transcript:plan_projection_measured` | 2 | 1.791 s |
| `transcript:measure_all_heights` | 3 | 1.788 s |
| `render:tool_call` | 256 | 1.658 s |
| `render:inline_diff_cached` | 256 | 1.648 s |
| `render:code_block` | 1024 | 876 ms |
| `render:exec` | 400 | 382 ms |
| `render:wrapped_output` | 400 | 367 ms |

Conclusion: this slice intentionally centralizes behavior without changing the rendered-measurement architecture, so the same hotspots remain. The small allocation/time movement is within this path's current full-render cost profile; the next slices still need width-independent IR measurement to remove the expensive work.

### Phase 2: Markdown IR with `pulldown-cmark`

**Goal:** replace the custom markdown parser with `pulldown-cmark` and build a Smelt AST from its events; keep all Smelt rendering decisions.

- Add dependency to `smelt_core`:
  ```toml
  pulldown-cmark = { version = "0.13", default-features = false }
  ```
- Define `MarkdownBlock` / `MarkdownNode` / `InlineSpan` AST as described in §3.3.
- Build a translator from `pulldown-cmark::Parser::into_offset_iter()` to `MarkdownBlock`:
  - paragraph, heading, blockquote, list, code block, table, rule
  - emphasis, strong, strikethrough, inline code, link, image, task list marker
  - source byte ranges for nodes, inline spans, table rows/cells, and code blocks
- Implement `measure_markdown(ast: &MarkdownBlock, width: u16) -> RowIndex`.
- Implement `render_markdown(ast: &MarkdownBlock, width: u16, rows: Range<RowIndex>, out: &mut RowSink)`.
- Preserve existing Smelt rendering behavior: no blank line after headings, custom table layout (dynamic column widths, fallback, borders, cell alignment, wrapping inside cells), block/inline code chrome, list indentation, blockquote bars.
- Preserve `source_text` / copy-yank semantics so copying a rendered table or chrome-delimited block returns the original markdown.
- Move heading adjacency/block-gap behavior to AST semantics instead of the old raw `trim_start().starts_with('#')` heuristic. This intentionally fixes escaped headings and invalid heading markers.
- **Before replacing the old parser, add regression tests** that capture current rendering for:
  - tables with inline code, emphasis, and wrapped cell content
  - nested code blocks and fenced blocks inside lists/blockquotes
  - headings immediately followed by paragraphs/lists
  - lists, blockquotes, links, images, task lists, strikethrough
  - copy/yank returning original markdown for the above
- Run the new AST renderer against those tests. Where the old renderer was buggy, update the expected output to the correct behavior and document the fix.
- Replace `crates/tui/src/content/display_renderers/markdown.rs` with the new AST renderer.

**Deliverable:** markdown blocks are parsed once to AST and measured/rendered from it; table layout and custom formatting remain Smelt code; the old line-oriented parser is deleted.

**Completed slice:** markdown parsing now produces a width-independent structural IR for the blocks that previously forced custom parsing decisions.

- Added `pulldown-cmark` to `smelt_core` with default features disabled.
- Added `smelt_core::content::markdown_ir::{MarkdownBlock, MarkdownNode, parse_markdown}` in `crates/core/src/content/markdown_ir.rs`. This first IR slice borrows the source and keeps source ranges for plain source, fenced/indented code blocks, tables, and horizontal rules; code nodes also store language and body ranges, and table nodes store parser-derived alignments and cell boundaries.
- Updated `crates/tui/src/content/display_renderers/markdown.rs` to render from `MarkdownBlock` while preserving the existing Smelt line renderer for source ranges and the existing code/rule renderers for specialized nodes. Table rendering now consumes parser-derived rows instead of reparsing pipe-delimited source lines, and headings, paragraphs, blockquotes, and list blocks are classified from `pulldown-cmark` block events instead of line-prefix detection.
- Kept current copy/source behavior for rendered markdown tables and current spacing behavior for headings, lists, code blocks, tables, and horizontal rules. Ordinary source lines still use Smelt's line-level styling, but block classification now comes from `pulldown-cmark` where possible, and inline emphasis/code/strike/link/autolink parsing now comes from `pulldown-cmark` instead of Smelt's old custom delimiter parser.
- The old backtick-fence helpers remain in `smelt_core::content` for now because they are tested parsing primitives, but the transcript markdown renderer no longer uses them.

Validation:

```bash
cargo test -p smelt-core markdown_ir
cargo test -p smelt-tui markdown
cargo test -p smelt-tui transcript_buf
cargo test -p smelt-core -- --test-threads=1
cargo test -p smelt-tui
cargo test -p smelt-tui mixed_large_transcript_projection_baseline -- --ignored --nocapture
```

Baseline result after the markdown IR parser slice:

```text
TRANSCRIPT_LAYOUT_BASELINE input_bytes=10497943 generated_bytes=10499021 blocks=3404 total_rows=120137 diff_caches=128 diff_cache_ms=1698 resize_diff_caches=128 resize_diff_cache_ms=1682 first_ms=843 resize_ms=931 visible_ms=3 allocs=3387644 bytes_allocated=644856584 visible_rows=80
```

Top duration totals from the same run:

| label | count | total |
|---|---:|---:|
| `render:text` | 1024 | 3.703 s |
| `render:markdown` | 1024 | 3.695 s |
| `render:build_diff_cache` | 256 | 3.369 s |
| `transcript:plan_projection_measured` | 2 | 1.773 s |
| `transcript:measure_all_heights` | 3 | 1.773 s |
| `render:tool_call` | 256 | 1.606 s |
| `render:inline_diff_cached` | 256 | 1.597 s |
| `render:code_block` | 1024 | 919 ms |
| `render:exec` | 400 | 376 ms |
| `render:wrapped_output` | 400 | 361 ms |

Conclusion: this slice moves markdown structural parsing into a reusable core IR and keeps rendering compatibility, but it still renders full blocks for measurement. The measured hot path is therefore intentionally unchanged until markdown/code/table measurement can read the IR directly.

### Phase 3: Code block IR and separate syntax render cache

**Goal:** code block height does not run syntect, and DisplayIR stays pure/serializable.

- Introduce `CodeBlock` IR with `lines: Vec<InlineLine>` and `lang: Option<String>`.
- Do not embed `OnceCell<Vec<SyntaxLine>>` inside the IR. Syntax highlighting is an ephemeral render cache owned by `TranscriptProjection` / renderer.
- Add `RenderCaches::syntax`, keyed by block content hash, language, and theme/syntax version.
- Split `render_code_block` into:
  - `parse_code_block(lines, lang) -> CodeBlock`
  - `measure_code_block(block: &CodeBlock, width: u16) -> RowIndex`
  - `render_code_block(block: &CodeBlock, width: u16, rows: Range<RowIndex>, theme, caches, out)`
- Syntax tokens are computed only when visible rows require them. If a highlighter needs prior-line state, computing from the start of the visible code block is acceptable initially; add incremental checkpoints only if profiling shows huge code blocks are a problem.

**Deliverable:** code block measurement is syntect-free; DisplayIR contains no theme-dependent or render-cache state.

**Completed slice:** code blocks now have a pure width-independent IR, and streamed code-line height measurement no longer renders buffers or runs syntect.

- Added `smelt_core::content::code_block::{CodeBlock, parse_code_block, measure_code_block}` in `crates/core/src/content/code_block.rs`. `CodeBlock` stores language plus tab-expanded `InlineLine<()>` rows using preserved-space wrapping; measurement sums `InlineLine::wrap_rows(width.max(1))` and does not touch syntax highlighting.
- Updated `crates/core/src/content/highlight/syntax.rs` so `render_code_block` consumes `CodeBlock` instead of raw lines. Rendering still computes syntect spans as a visible-render concern, but parsing/tab expansion and line wrapping now share the same IR as measurement.
- Updated markdown fenced-code and streamed code-line renderers to parse `CodeBlock` before rendering (`crates/tui/src/content/display_renderers/markdown.rs`, `crates/tui/src/content/display_block.rs`).
- Added a direct `Block::CodeLine` height path in `crates/tui/src/content/transcript_buf.rs`, including view-state and block-gap accounting, before falling back to `RenderedBlockCache` for other block variants. Code-line exact heights now leave `rendered_block_cache_len()` at zero and increment only the exact-height measurement counter in coverage.
- The separate syntax render cache and row-range code-block renderer are still future work. This slice removes syntect/rendered-buffer measurement for `Block::CodeLine` while keeping the existing full-render path for markdown blocks and visible rendering.

Validation:

```bash
cargo test -p smelt-core code_block
cargo test -p smelt-tui markdown
cargo test -p smelt-tui code_line_heights_measure_without_rendering_syntax
cargo test -p smelt-tui transcript_buf
cargo test -p smelt-tui
cargo test -p smelt-core -- --test-threads=1
cargo fmt
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p smelt-tui mixed_large_transcript_projection_baseline -- --ignored --nocapture
```

Baseline result after the code block IR slice:

```text
TRANSCRIPT_LAYOUT_BASELINE input_bytes=10497943 generated_bytes=10499021 blocks=3404 total_rows=120137 diff_caches=128 diff_cache_ms=1685 resize_diff_caches=128 resize_diff_cache_ms=1725 first_ms=912 resize_ms=1014 visible_ms=3 allocs=3393237 bytes_allocated=640539440 visible_rows=80
```

Top duration totals from the same run:

| label | count | total |
|---|---:|---:|
| `render:text` | 1024 | 4.055 s |
| `render:markdown` | 1024 | 4.039 s |
| `render:build_diff_cache` | 256 | 3.399 s |
| `transcript:plan_projection_measured` | 2 | 1.925 s |
| `transcript:measure_all_heights` | 3 | 1.924 s |
| `render:tool_call` | 256 | 1.691 s |
| `render:inline_diff_cached` | 256 | 1.681 s |
| `render:code_block` | 1024 | 972 ms |
| `render:exec` | 400 | 398 ms |
| `render:wrapped_output` | 400 | 383 ms |

Conclusion: the large mixed baseline is still dominated by full markdown/tool/diff measurement and remains broadly comparable to the previous slices because the synthetic code fences live inside markdown blocks. The meaningful change is architectural and covered directly: standalone `Block::CodeLine` heights are now measured from IR without populating rendered block caches or running syntax highlighting.

### Phase 4: Diff / file view IR

**Goal:** diffs are measured without building full styled caches.

- Evolve `CachedInlineDiff` into `DiffIr`:
  - store diff structure, line numbers, expanded text, and syntax extension,
  - keep syntax style ranges and width-derived layout caches out of the IR; wrap from text for measurement and use ephemeral syntax state for rendering.
- Provide:
  - `build_diff_ir(old, new, path, anchor, lang) -> DiffIr`
  - `measure_diff(ir: &DiffIr, width: u16, gutter: GutterStyle) -> RowIndex`
  - `render_diff(ir: &DiffIr, width: u16, rows: Range<RowIndex>, gutter, theme, out)`
- Update `source_view.rs` and `diff.rs` to use the IR.
- Ensure `DiffIr` is cached per diff content, not per width.

**Deliverable:** diff measurement does not build styled caches; resize does not rebuild diff caches.

**Completed slice:** `CachedInlineDiff` became serializable `DiffIr` in `crates/core/src/content/highlight/diff.rs`; it stores diff structure, expanded text, line numbers, and syntax extension without persistent syntax style ranges or duplicated layout text. Syntax style spans are computed in `print_diff_ir` during rendering. `measure_diff_ir` measures wrapped rows from the IR without syntect, and `print_diff_ir` now applies `skip`/`max_rows` to visual rows so measurement and rendering use the same row model. File-view leaves compile to the same IR via `build_file_view_ir`, and rendered layouts made only of IR/spec leaves are marked width-independent so resize can reuse them instead of rerunning tool extraction. Validation: `cargo nextest run --workspace` (3043 passed, 1 skipped) and `cargo clippy --workspace --all-targets -- -D warnings`.

### Phase 5: Tool body IR and global tool prerender removal — complete

**Goal:** historical tool blocks are cheap to measure; tool render hooks run only on content changes or DisplayIR cache misses, never because width changed.

Completed slice:

- Introduced serializable `ToolBody` / `LayoutIr` with `Text` and `DiffIr` leaves.
- Replaced `ToolState.render_cache` with width-independent `ToolState.body`.
- Added native declarative Lua `smelt.layout.text` and `smelt.layout.tool_output`; bundled text/diff/file-view tool renderers now compile to `ToolBody` without persistent buffers.
- Removed the render-loop call that prerendered every transcript tool block at the current width.
- Projection now computes tool bodies only for the planned visible block window, stores them as width-independent sidecar display state, then replans against the updated row index when new bodies were stored.
- Tool body compilation is all-or-nothing; unsupported buffer leaves reject the body instead of silently dropping children.
- Tool block measurement reads `ToolBody` directly, including text wrapping and capped diff/file-view IR measurement, before falling back to raw output for tools without a compiled body.
- Visible materialization renders `ToolBody` directly; width changes rewrap the body and do not rerun Lua render hooks for hidden tools with cached bodies.
- Permission previews render through the same tool-body IR path; the old `RenderedLayout` / `extract_rendered_layout` pipeline was removed.
- Removed `ctx.width` from `ToolRenderCtx`; regenerated Lua API docs/stubs.

Validation:

- `cargo test -p smelt-core diff`
- `cargo test -p smelt-tui transcript_buf`
- `cargo test -p smelt-tui display_renderers`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo nextest run --workspace` → `3043 passed, 1 skipped`

**Deliverable:** resize does not rerun Lua tool render hooks for hidden tools.

### Phase 6: Unified `DisplayBlock` and `TranscriptProjection` rewrite — complete

**Goal:** one IR per block, one measurement path, one render path.

Completed slice:

- Added `DisplayBlock`, `DisplayModel`, `DisplayCacheKey`, `MeasureCtx`, and `RenderCtx` in `crates/tui/src/content/display_block.rs`.
- `TranscriptProjection` now compiles blocks into the width-independent display model, measures through `measure_block`, and materializes visible/full/range rows through `render_block_into`.
- Removed `RenderedBlockCache` and `crates/tui/src/content/block_buffers.rs`; width changes invalidate row indexes/materialized rows but keep compiled display blocks.
- Standalone code-line and tool-call measurement use their existing pure IR/body paths from `DisplayBlock`; remaining legacy block renderers are reached only through the unified display-block facade while they are migrated to pure row-range renderers.
- Updated projection tests to assert display-block compilation and width-independent reuse instead of rendered-buffer cache behavior.

Validation:

- `cargo test -p smelt-tui transcript_buf`
- `cargo test -p smelt-tui display_renderers`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo nextest run --workspace` → `3046 passed, 1 skipped`

**Deliverable:** transcript projection is display-model based end to end.

### Phase 7: Cleanup dead abstractions — complete

**Goal:** remove leftover scaffolding and simplify.

Completed slice:

- Removed the rendered-block cache abstraction in Phase 6; no `RenderedBlockCache` code remains.
- Collapsed transitional fake `DisplayBlock` variants into an explicit `Legacy` variant plus real `CodeLine` / `ToolCall` IR-backed variants.
- Removed leftover fixed-size rendered-cache batching from transcript measurement/materialization.
- Renamed `measure_all_heights` to `rebuild_row_index`; the method now describes the row-index side effect and no longer accepts theme state.
- Kept `BlockLayout<BufId>` intentionally as the raw Lua-returned tool preview/render shape. `BlockLayout<Box<Buffer>>`, `RenderedLayout`, `ToolState.render_cache`, and `CachedInlineDiff` are gone.
- No Lua API docs/stubs changed in this phase.

Validation:

- `cargo test -p smelt-tui transcript_buf`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo nextest run --workspace` → `3046 passed, 1 skipped`

**Deliverable:** codebase has fewer files and concepts; all tests pass.

### Phase 8: Implement `session.ir.bin` — complete

**Goal:** resume large sessions without reparsing/recompiling every block.

Disk persistence is required for the target UX and has been a DisplayIR design constraint from the start. This phase wires the already-serializable DisplayIR into the on-disk sidecar. The canonical session JSON remains the source of truth; DisplayIR is stored in a separate disposable cache file next to the session:

```text
sessions/<id>/session.json
sessions/<id>/session.ir.bin
```

Completed slice:

- Added binary `session.ir.bin` persistence with fixed magic bytes, cache format version, renderer version, Smelt build version, and payload length checks.
- Persisted derived display-cache data from the TUI persister alongside `session.json`; corrupt, missing, partial, renderer-mismatched, and build-mismatched sidecars are treated as cache misses.
- Hydrated display caches for both full session resume and resume-picker previews.
- Switched tool-call sidecar keys from in-memory layout revisions to stable serialized tool display state, including cached tool bodies, so warm resumes can skip Lua tool-body rendering for unchanged historical tool calls.
- Kept the sidecar disposable: no migration layer and no theme-, width-, or visible-row-dependent data is stored.
- Added unit coverage for sidecar round-tripping, corruption handling, display-model hydration, and cached tool-body hydration.

Validation:

- `cargo test -p smelt-tui display_cache`
- `cargo test -p smelt-tui display_block`
- `cargo test -p smelt-tui transcript_buf`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo nextest run --workspace` → `3050 passed, 1 skipped`

**Deliverable:** resume/preview can hydrate unchanged derived display data from `session.ir.bin`; cache misses rebuild from canonical `session.json`.

### Phase 9: Performance measurement and fixes

**Goal:** use measurements on 10 MB and 100 MB transcripts to find the remaining bottlenecks, then fix the ones that are worth the added code. This phase is for evidence-driven performance work, not speculative caps, trims, or representation tweaks that lose transcript content.

Completed in this slice:

- `ExactRowIndex` syncs itself when the existing order remains a stable prefix, so appended blocks do not force historical row remeasurement; order-prefix changes still fall back to a full index rebuild.
- Cache-miss compilation now flows through independent `CompileJob`s from `collect_compile_jobs`; those jobs can be scheduled in parallel once profiling identifies the right threshold and executor. The current caller keeps sequential execution to avoid adding scheduler complexity before it is needed.
- Synthetic 10 MB baseline improved from `first_ms=4394 resize_ms=4849` to `first_ms=2415 resize_ms=2785`, with visible projection at `visible_ms=3`.
- Resume tracing identified three remaining concrete costs in real 11 MB sessions: stale `session.ir.bin` decoded as bincode failure instead of a version miss, exact row-index rebuilds repeatedly measuring unchanged historical blocks, and unconditional session/display-cache writes on resume+quit.
- The display cache format now persists cached tool bodies and exact row-index entries keyed by width, `show_thinking`, ordered `BlockId`s, and per-block `LayoutKey`s. Hydration validates every cached node against current history before installing the prefix index, so exact row totals can be restored without a full measurement pass.
- Session saves now fingerprint the timestamp-normalized session snapshot plus display cache and skip unchanged writes when there are no image blobs to flush. This keeps resume+quit from rewriting `session.json` and `session.ir.bin` just because `updated_at_ms` would have changed.
- Added traces for display-cache row-index read/write counts, row-index cache hydration/rejection/miss, row-index generation/reuse, and the first/last missing block indexes during row-index rebuild.

Next measurements:

- Run the synthetic baseline at 100 MB and compare resume, resize, visible projection, allocation count, and bytes allocated.
- If display compilation is a material bottleneck, schedule `CompileJob`s in parallel with a measured block-count/byte threshold.
- If `InlineLine` allocation or width walking is a material bottleneck, optimize the representation based on allocation/profiling data.
- If visible rendering of enormous code/file-view blocks is a material bottleneck, add syntax-cache checkpoints.

**Deliverable:** 100 MB performance bottlenecks are measured, high-value fixes are implemented, and any rejected optimization is rejected because measurements show it is not worth the complexity or because it would lose transcript content.

### Phase 10: Eliminate legacy display paths

**Goal:** finish the structural migration so transcript layout is a single IR-driven pipeline instead of a mix of `DisplayBlock` IR and legacy parser/render fallbacks.

Completed:

- `DisplayBlock::Legacy` is gone. Transcript display ownership is now structurally typed, and production measurement/rendering dispatches over explicit `DisplayBlock` variants.
- Tool-body hydration validates cached derived entries against the current `Block::ToolCall` shape before installing them, preventing stale cache entries from crossing block boundaries.
- Parser-level production measurement/render fallbacks were removed; parser tests now exercise the compiled display-block path through `compile_block` and `render_block_into`.
- Parser renderer visibility was narrowed, and the unused test-only catch-all renderer/code-line shim was deleted.
- `build_rows` remains as an explicit full-transcript compatibility API; range consumers use exact row indexes plus intersecting block materialization.

Validation:

- `cargo fmt --check`
- `cargo test -p smelt-tui display_cache`
- `cargo test -p smelt-tui display_renderers`
- `cargo test -p smelt-core transcript_model`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo nextest run --workspace` → `3059 passed, 1 leaky, 1 skipped`

**Deliverable:** complete. `DisplayBlock::Legacy` is gone, display renderer modules no longer own transcript layout, and the remaining full-row paths are intentional API choices rather than hidden measurement dependencies.

### Phase 11: Simplify materialization and clean up post-migration boundaries

**Goal:** remove leftover viewport/materialization magic numbers and simplify names/boundaries that still reflected the old parser-owned transcript layout model. Avoid bigger rendering abstractions until profiling proves they are worth the complexity.

Resume profile after Phase 10/early Phase 11:

- Resume-only visible projection is no longer the bottleneck: `transcript:project_visible_range` is ~3 ms, row-index rebuild/hydration is sub-millisecond, and `tool:prerender_bodies` does no hidden work.
- Cold resume is now mostly data loading/reconstruction: `transcript:build_from_session` ~85-90 ms, JSON parse/read ~35-45 ms, and `session_ir` read/decode ~55 ms.
- Resume + scroll + resize still shows occasional large materialization spikes when the viewport intersects a very large tool/diff block, but block-local row rendering is intentionally deferred for now because it would add a larger abstraction. The current cleanup keeps the code easier to reason about before taking that step.

Completed:

- Removed fixed transcript head/tail overscan constants (`20` rows each) from projection planning.
- Visible-row and tail projections now preload half a viewport around the exact visible window. This preserves nearby-scroll reuse without tying materialization to an arbitrary global constant.
- Added regression coverage that small viewports materialize a viewport-relative bounded window rather than inheriting a fixed large cushion.
- Renamed `content::transcript_parsers` to `content::display_renderers`; these modules are display renderers for compiled `DisplayBlock`s, not owners of transcript parsing/layout.
- Split explicit tool-body cache APIs out of normal tool-state mutation: runtime tool-body installation bumps history generation only when the display hash changes, while display-cache hydration restores derived bodies without treating it as canonical transcript mutation.
- Refactored session persistence setup into small helpers for pending image blobs and the persist snapshot, making `save_session`'s skip/write decisions easier to audit.
- Picker overscan remains unchanged because it is independent list virtualization, not transcript exact-row planning.

Validation:

- `cargo fmt --check`
- `cargo test -p smelt-core transcript_model`
- `cargo test -p smelt-tui display_renderers`
- `cargo test -p smelt-tui display_cache`
- `cargo test -p smelt-tui transcript_buf`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo nextest run --workspace` → `3062 passed, 1 skipped`

**Deliverable:** complete. Transcript projection uses exact row heights plus viewport-relative preload; post-legacy renderer naming matches ownership; derived tool-body cache hydration no longer masquerades as normal transcript mutation.

### Phase 12: Make `session.ir.bin` a derived-data binary cache

**Goal:** keep `session.json` as the only serialized transcript and make `session.ir.bin` a disposable derived-data sidecar with a binary payload.

Completed:

- Changed `DisplayCacheData` from serialized full display-block entries to derived tool-body entries plus exact row-index entries.
- Tool-body cache entries store only the tool block id, call id, display cache key, and cached `ToolBody`. Non-tool blocks are compiled from canonical `BlockHistory` on demand instead of being duplicated in the sidecar.
- Hydration installs cached tool bodies into `BlockHistory` only after validating the current block is the same tool call and the candidate state hashes to the cached display key.
- Row-index hydration remains after tool-body hydration so row-index keys can validate against the restored sidecar hashes.
- Replaced the JSON payload inside the binary sidecar with `bincode`; the fixed header remains. The cache format version was intentionally not bumped because old sidecars will be manually removed for this branch.
- Updated persist/session-IR telemetry and tests to report tool-body counts rather than generic display entries.

Validation:

- `cargo fmt --check`
- `cargo test -p smelt-tui display_cache`
- `cargo test -p smelt-tui display_block`
- `cargo test -p smelt-tui transcript_buf`
- `cargo test -p smelt-core transcript_model`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo nextest run --workspace` → `3062 passed, 1 skipped`

**Deliverable:** complete. `session.ir.bin` now stores only derived tool-body and row-index cache data in a binary payload; deleting it loses no canonical transcript state and cache misses rebuild from `session.json`.

---

## 7. How each user-facing path changes

### Resume picker preview

Today:

```
load session → build transcript → prerender all tools → measure all heights by rendering → render visible tail
```

Goal:

```
load session → hydrate DisplayIR cache → compile cache misses → measure all heights cheaply → render visible tail
```

Expected: ~10-50 ms for an 11 MB session instead of ~2 s.

### Session resume

Today: same as preview plus full first-frame render flush.

Goal: hydrate DisplayIR cache during `load_session`, compile misses, then cheap measure + visible render.

### Resize

Today:

```
width change → clear rendered cache → prerender all tools → render all blocks to measure → render visible
```

Goal:

```
width change → recompute heights from IR → render visible
```

No Lua hooks, no syntect, no diff cache rebuilds for hidden blocks.

### Streaming a new block

Today: block is appended; next frame measures by rendering.

Goal: compile IR for the new block once, append height to row index, render visible tail.

### Copy / yank / search

Current status: `display_rows_for_range` and `copy_range` now rebuild the exact row index and materialize only the intersecting block range. `build_rows` remains a full-materialization compatibility path for consumers that need the entire transcript as strings.

Long-term goal: all three paths render/copy directly from IR row ranges, with `build_rows` kept only if a full-transcript API remains necessary.

---

## 8. Testing strategy

### Invariants to test

1. **Exactness:** for every block type, sample of widths, and sample of view states:
   ```
   measure(width) == render(block, width, 0..measure(width)).len()
   ```
2. **Row index correctness:** for random transcripts and widths, prefix rows match sequential measurement.
3. **Visible materialization coverage:** for random scroll positions, `project_planned` materializes rows covering `[scroll_top, scroll_top + viewport_rows)`.
4. **Resize equivalence:** rendering at width W1 then W2 produces the same rows as rendering directly at W2.
5. **Copy/yank correctness:** `copy_range` returns the same text as the full rendered transcript for the same range.
6. **Persistent cache correctness:** loading DisplayIR from disk renders/measures identically to compiling it fresh.
7. **AST gap semantics:** heading adjacency uses parsed markdown headings, not raw `#` prefix heuristics.

### Regression tests

- Add regression tests **before** replacing the markdown parser. Capture current rendering of tables, nested code blocks, headings, lists, blockquotes, links, inline emphasis, task lists, and `source_text` / copy-yank behavior.
- When the AST renderer produces different output, investigate whether the old behavior was a bug. Fix the expected output and document the change rather than preserving bugs for compatibility.
- Existing storybook tests for transcript rendering must still pass.
- Existing tests for diff rendering, markdown tables, code blocks, tool output must pass.
- Add large synthetic session benchmark.

### Fuzz/property tests

- Random markdown strings → parse → measure → render → compare line count.
- Random terminal widths → ensure no panic and monotonic total rows.

---

## 9. Risks and mitigations

| Risk | Mitigation |
|------|------------|
| Breaking plugin tool renderers | Accepted. Option A is a breaking declarative width-independent API; migrate built-in tools and update docs/stubs. No compatibility shim. |
| Table rendering fidelity lost | `pulldown-cmark` only parses rows; all layout logic stays in Smelt; regression tests for tables before migration |
| `source_text` / copy-yank drift | Build markdown AST with source ranges via `into_offset_iter`; reproduce `LineBuilder::set_source_text`, `arm_source_text`, `stamp_copy_group`, `stamp_chrome_delimited_block`; regression tests |
| Unicode/ANSI wrapping drift | Centralize in `InlineLine`; exhaustive tests |
| Syntax highlighting cache complexity | Keep syntax cache separate from DisplayIR; key by content/language/theme; compute only for visible rows |
| Persistent cache corruption/staleness | Cache is disposable; version/hash mismatch and read errors are cache misses |
| Memory blow from IR for 100 MB | Keep IR proportional to source; profile allocations and optimize representation or chunk/checkpoint without dropping transcript content |
| Long migration | Each phase is independent and testable; ship after each phase |
| Theme changes | IR is width-independent; theme changes clear render caches but not measurement or persistent IR |
| Selection/copy over wrapped rows | Render the requested range from IR; use same wrapping and row-decoration logic |

---

## 10. What to delete / rename

### Completed deletions / renames

- `DisplayBlock::Legacy` and the old `layout_block_into` path were removed.
- `transcript_parsers` module → `display_renderers`; these modules remain as renderer implementations for compiled `DisplayBlock`s.

### Deferred deletions / renames

- `BlockLayout` → `LayoutIr` if the Lua-returned tool preview/render shape is fully replaced.
- `RenderedRowCache` (if optional visible-row cache is reintroduced)

### Keep

- `BlockHistory`
- `LayoutKey`
- `ExactRowIndex`
- `TranscriptProjection` (rewritten)
- `TranscriptView` facade
- `SourceViewTarget` idea

---

## 11. Success criteria

1. Resume picker opens in < 100 ms for an 11 MB session.
2. Resuming an 11 MB session with a warm DisplayIR cache shows the transcript in < 100 ms.
3. Terminal resize of an 11 MB session is < 50 ms.
4. A 100 MB synthetic session is usable (resume/resize < 500 ms, scrolling smooth).
5. All existing rendering tests pass.
6. Codebase has fewer concepts/files than before (net deletion).
7. New architecture is documented and obvious to future maintainers.
8. Warm resume uses the persistent DisplayIR cache; missing/stale/corrupt cache falls back to correct recompilation.

---

## 12. Summary of architectural shift

| Aspect | Before | After |
|--------|--------|-------|
| Block model | `Block` enum | `Block` + compiled `DisplayBlock` IR |
| Height measurement | render into Buffer, count lines | `DisplayBlock.measure(width)` |
| Width handling | everything invalidated | only wrapping recomputed |
| Tool rendering | Lua hook per width | Lua returns width-independent declarative IR per content change |
| Diff rendering | build styled cache per width | `DiffIr` once; syntax cache separate/render-only |
| Markdown | parse+emit every time | parse once to source-ranged AST; persist DisplayIR cache |
| Global prerender | all tools every frame | only visible rows rendered |
| Caches | several overlapping caches (rendered blocks, tool cache, diff cache) | persistent DisplayIR cache + ephemeral render caches |

The end state is a single pipeline:

```
session → hydrate persistent IR cache → compile misses → measure cheaply per width → index rows → render only visible rows
```

This is the architecture that can scale.
