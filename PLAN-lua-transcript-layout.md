# Lua-Defined Transcript Layout Plan

## Goal

Move transcript visual policy out of hardcoded Rust block renderers and into bundled/user-overridable Lua renderers, while preserving the transcript layout rewrite principles:

- exact row counts, scrollbar, search, copy/yank, and vim navigation;
- no visible UI/state regressions;
- no width-dependent Lua render callbacks;
- no buffer-leaf compatibility path;
- one serializable, width-independent DisplayIR/LayoutIR;
- Rust owns measurement/rendering mechanics, Lua owns display policy.

The target architecture is:

```text
BlockHistory canonical state
  -> Lua renderer callback returns declarative LayoutIr
  -> Rust compiles/caches LayoutIr as DisplayIR
  -> Rust measures every block cheaply and exactly
  -> Rust renders only visible row ranges
```

Lua must not paint terminal rows directly in the transcript hot path. Lua describes layout; Rust measures and paints it.

## Planning principles

These carry forward from the transcript layout rewrite and apply to this Lua-layout step as well:

1. **This plan is a direction, not a contract.** If a better approach emerges during implementation, take it and update this document.
2. **Do not defer work just because it is large.** If something is worth doing for the end-state quality of the code, do it now.
3. **Better/simpler code is the goal.** The final code must be simpler, less complex, more composable, more robust, and more testable than what exists today.
4. **APIs can break and evolve.** Do not keep old APIs, internal shims, backward compatibility layers, or Lua surface compatibility for their own sake.
5. **Remove before adding.** If an abstraction no longer pulls its weight, delete it rather than wrap it.
6. **Refactor/consolidate along the way.** If cleanup is worth doing for the final architecture, do it as part of the migration instead of deferring it because it is large.
7. **Exactness is non-negotiable.** Speed must come from avoiding wasted work, not from faking data or estimating rows.
8. **Production names describe enduring behavior, not migration bookkeeping.** Phase labels belong in this plan and task notes, not in production identifiers, comments, committed test names, or public APIs.
9. **Keep this plan current as work lands.** Mark completed slices with code references, baseline numbers, and changed conclusions so later phases start from current truth rather than stale intent.

## Design principles

1. **One canonical source state.** `Session` / `BlockHistory` remain the canonical transcript state. LayoutIR/DisplayIR is derived display data, not a replacement for semantic history.
2. **DisplayIR is serializable from day one.** Persistent cache storage may land later, but every IR type must be cache-safe: width-independent, theme-independent, deterministic, and free of Lua handles, buffers, render caches, and non-serializable state.
3. **Width only affects wrapping and row selection.** Compile Lua layout once per semantic/display-policy change, not per width.
4. **Theme only affects style resolution.** Lua may name highlight groups; Rust resolves them at render time without rebuilding IR.
5. **Rendering is a function `IR × width × row_range → rows`.** No hidden global state should be required to paint visible rows.
6. **Measurement is real layout, not rendering.** Measuring must not allocate terminal buffers, run syntect, call Lua, or emit styled terminal spans.
7. **Visible rendering uses the same IR as measurement.** Measurement and rendering semantics must stay exact for scrollbar, vim navigation, copy/yank, search, and previews.
8. **Keep `ExactRowIndex`; change how it is fed.** Prefix-row indexing is a good abstraction. It should consume exact cheap measurements from IR instead of rendered buffers.
9. **Lua renderers run only for semantic/display-policy changes.** Width changes, theme changes, scrolling, and hidden historical blocks with valid IR must not call Lua.
10. **No approximate previews.** Dialog previews and live transcript projection use the same primitives and exact row semantics.

## Non-negotiables

1. **No UI/state regressions.** Current status colors, labels, gutters, copy/search semantics, hidden thinking behavior, denied tool behavior, etc. must be preserved unless intentionally changed with tests/snapshots.
2. **Exactness remains non-negotiable.** Speed comes from cached width-independent IR and visible-row rendering, not estimated heights.
3. **Width changes must not call Lua.** Lua output is width-independent. Rust owns wrapping, measurement, clipping, syntax rendering, and row-range rendering.
4. **Theme changes must not call Lua.** Lua may reference theme group names; Rust resolves groups at render time.
5. **Renderer callbacks are pure layout builders.** No UI mutation, transcript mutation, async work, terminal width reads, buffer handles, or side effects required for rendering correctness.
6. **Breaking Lua API changes are allowed.** This is alpha/greenfield. Delete old APIs instead of preserving shims.
7. **Bundled Lua defaults are product code.** Moving behavior to Lua does not mean simplifying it away.

## Boundary

Lua owns:

- full-block transcript render policy for every block kind;
- labels, icons, separators, grouping, ordering;
- block-specific and tool-specific dispatch, as ordinary Lua composition inside the root renderer/defaults;
- deciding whether to use markdown/code/diff/file/panel/gutter/cap primitives.

Rust owns:

- canonical transcript/session state;
- declarative layout primitive implementation;
- DisplayIR serialization and cache invalidation;
- exact measurement;
- visible row rendering;
- row metadata: selectable, copy_as, source_text, soft-wrap, preformatted, fill bg;
- syntax highlighting and diff/file rendering mechanics;
- fallbacks on Lua renderer error.

No transcript primitive should have hidden context-specific indentation. Today tool-body rendering passes an implicit `with_gutter` flag into leaves. The target design should remove that seam: Lua composes `layout.gutter(...)` explicitly, and Rust primitives render the same IR in transcript and dialog/preview surfaces with only the render target width/theme changing.

Dialog previews and transcript bodies must share the same primitive implementations. A diff/file preview may be gutterless in a permission dialog and guttered in the transcript because Lua chose different surrounding chrome, not because Rust has two ad-hoc renderers.

## API surface boundaries

Smelt will still have both declarative and imperative UI APIs, but their scopes must not blur.

Declarative LayoutIR is the only accepted output for transcript renderers, bundled transcript defaults, and confirm/preview content that needs exact copy/search/scroll semantics. Transcript renderers must not return buffers, windows, paint callbacks, terminal spans, or imperative drawing functions.

Imperative buffer/render/paint APIs remain useful as low-level escape hatches for interactive custom windows, overlays, prompt/status bars, pickers, debug panels, and experiments. They should not become part of transcript display policy or projection measurement.

Keep the namespace distinction explicit:

- `smelt.ui.layout` composes windows/screen regions;
- `smelt.layout` is the declarative content/block LayoutIR API for transcript/preview surfaces, unless renamed before implementation;
- `smelt.render`, buffer methods, and `smelt.paint` are imperative window/overlay drawing APIs.

The naming is still a pressure point because `smelt.layout` and `smelt.ui.layout` are easy to confuse. If the API grows beyond transcript content, consider a clearer namespace such as `smelt.content.layout`, `smelt.display`, `smelt.ir`, or `smelt.view` before stabilizing docs/stubs.

## Current code audit notes

The current branch is not a blank slate. These are the concrete seams the migration must remove or preserve deliberately:

- `DisplayBlock` still hardcodes every block variant and dispatches to per-block Rust renderers. The target should replace `DisplayBlock::{User, Text, ToolCall, ...}` with cached `LayoutIr` plus minimal semantic fallback constructors.
- Renderer-produced `ToolBody` is no longer stored on canonical `ToolState` / `BlockHistory`. Semantic tool state remains status, terminal elapsed, output, and user message; derived bodies live in `DisplayModel` and the `session.ir.bin` display cache.
- `ToolBody` is still the body-only bridge name until Phase 4 deletes that contract. `BlockLayout` now has display-oriented aliases (`LayoutIr`, `LayoutNode`, `LayoutLeaf`) and compiled source leaves use `SourceViewIr` instead of `IrLeaf::DiffIr`.
- Tool custom render is currently body-only: Rust still owns the `* name summary elapsed` chrome and denied/user-message behavior. Phase 3 must switch to full-block Lua defaults in one direction, then delete the body-only compatibility path.
- Tool preview and transcript body currently share `BlockLayout` values but differ through implicit Rust render context such as the tool-block base gutter. Replace this with explicit Lua chrome/gutter composition when full-block tool rendering lands.
- `smelt.layout.tool_output` has been deleted from the `smelt.layout` API. Raw tool output is now bundled Lua composition in `smelt.transcript.defaults.render_tool_output`, implemented as `layout.cap(layout.gutter(layout.text(..., { ansi = true })), { keep = "tail", marker = "above" })`.
- Compiled source-view leaves now use `SourceViewIr`. File views still reuse the diff/file-view renderer mechanics internally, but the public/display IR no longer stores them as `IrLeaf::DiffIr`.
- `render_ir_hbox` now measures child nodes directly and renders requested row ranges instead of rendering full child columns. The current bridge still uses a one-row temporary buffer per visible child row to preserve existing span clipping until row compositing is generalized.
- `layout.markdown` is not currently exposed as a text-backed placeholder. It should return real markdown LayoutIR when the full-block renderer migration needs it; until then bundled Lua uses `layout.text` for plain fallback bodies.
- Unit coverage now includes the motivating raw-output tail cap. Storybook still lacks a dedicated long-output tail case; add one before broader full-block renderer changes if visual coverage is needed.

## Design pressure points to settle before implementation

These are the areas most likely to create accidental complexity or hidden Rust policy if left vague:

1. **Root renderer composability.** One public `set_renderer(fn)` keeps Rust simple, but “last setter wins” can be hostile to plugins. Provide a small Lua-only middleware helper around the root renderer:
   `transcript.get_renderer()` / `transcript.set_renderer(fn)` / `transcript.extend_renderer(name, fn)`. This must not become a Rust-owned per-kind or per-tool registry.
2. **Intentional empty blocks.** `nil` should mean invalid renderer output/fallback, not “hide this block,” or user custom renderers cannot intentionally suppress a block. Add an explicit `layout.empty()` or equivalent zero-row node if hiding/collapsing is supported.
3. **Determinism and cache invalidation.** A root renderer can close over arbitrary Lua globals. The contract is that renderer output must be deterministic for `(block, ctx, renderer generation)`. `set_renderer`, `extend_renderer`, extension removal, and Lua reload bump generation; width/theme/scroll do not. Do not try to hash Lua closures. If user/plugin code mutates renderer-affecting closed-over state without reinstalling/reloading the renderer, it must call an explicit invalidation API that bumps renderer generation.
4. **Semantic block snapshot schema.** Define the snapshot shape before migrating blocks. It should be serializable/plain-data, versioned internally, and include enough precomputed semantic annotations that defaults do not need to duplicate Rust parsing behavior in Lua.
5. **Copy/search/source metadata as composable wrappers.** Custom renderers must not lose copy/yank/search semantics by accident. Make `layout.source`, `layout.copy_as`, `layout.selectable`, and row metadata behavior precise, including how wrappers compose through panels/gutters/caps.
6. **Dynamic primitives boundary.** Keep the dynamic set small. Pending elapsed needs a dynamic primitive so it can tick without Lua. Most other state changes (`status`, `output`, `user_message`, args) are semantic invalidations and should rerun Lua for that block.
7. **Panel/background/narrow-width semantics.** Cards/backgrounds are central to customization. Define padding, borders, bg fill, clipping, and degenerate widths before relying on snapshots.
8. **`hbox` width allocation.** Correct row-range rendering requires a precise column model: fixed/flex widths, gaps, min widths, overflow, and alignment. Do not implement hbox until this contract is written down.
9. **User-text and thinking-summary helpers.** Keep exact Rust mechanics for slash-command/ref/image highlighting and folded thinking summaries, but expose them as semantic data or narrow mechanical helpers. Do not hide whole block chrome behind Rust primitives.
10. **Default helper patchability.** Decide whether bundled defaults expose tool-body dispatch tables or only functions. Users can always replace the root renderer, but plugin cooperation may benefit from a Lua-level composition convention.
11. **IR size limits and abuse cases.** User renderers can generate huge trees. Add validation limits/error messages for node count, depth, span count, and string sizes so a bad renderer cannot make projection pathological.
12. **Preview/transcript context differences.** Dialogs and transcript should share primitives, but they may pass different `ctx.surface` or default chrome. Keep that difference explicit in Lua, not hidden in Rust render options.

## Proposed Lua API

### Layout primitives

`smelt.layout` becomes the single declarative display API. The current `layout.leaf(buf)` buffer-leaf primitive is removed.

Target primitive set:

```lua
layout.text(content, opts?)
layout.runs(spans, opts?)
layout.line(spans, opts?)           -- one-row/clipped runs when exact one-row behavior is needed
layout.elapsed(ref, opts?)          -- dynamic elapsed text; Rust updates value without rerunning Lua
layout.markdown(source, opts?)
layout.code(source, opts?)
layout.diff(opts)
layout.file_view(opts)
layout.vbox(items, opts?)
layout.hbox(items, opts?)
layout.panel(child, opts?)
layout.gutter(child, opts?)
layout.separator(opts?)
layout.spacer(opts?)
layout.cap(child, opts?)           -- head/tail row capping with optional marker
layout.source(child, source_text)
layout.selectable(child, selectable)
layout.copy_as(child, text)
layout.style(child, opts)
layout.empty()                    -- explicit zero-row node; `nil` remains invalid/fallback
```

Bundled Lua may add policy helpers on top of those primitives, but helpers belong in `smelt.transcript.defaults`, not in `smelt.layout`. `smelt.layout` must not grow transcript-product helpers such as `layout.tool_header`, `layout.tool_output`, `layout.user_message`, or `layout.thinking_summary`.

Initial bundled defaults:

```lua
smelt.transcript.defaults.render(block, ctx)
smelt.transcript.defaults.render_tool(block, ctx)
smelt.transcript.defaults.render_tool_header(block, ctx, opts?)
smelt.transcript.defaults.render_tool_body(block, ctx, opts?)
smelt.transcript.defaults.render_tool_output(output, ctx, opts?)
smelt.transcript.defaults.render_user(block, ctx)
smelt.transcript.defaults.render_user_text(block, ctx)
smelt.transcript.defaults.render_assistant(block, ctx)
smelt.transcript.defaults.render_thinking(block, ctx)
smelt.transcript.defaults.render_thinking_summary(block, ctx)
smelt.transcript.defaults.render_exec(block, ctx)
smelt.transcript.defaults.render_mode(block, ctx)
smelt.transcript.defaults.render_process_status(block, ctx)
smelt.transcript.defaults.render_compacted(block, ctx)
```

These defaults are ordinary Lua functions. Users may call them, compose them, copy their structure, or ignore them entirely from their root renderer.

Examples:

```lua
layout.panel(layout.markdown(block.text), {
  hl = "SmeltUserBg",
  padding = 1,
})

layout.gutter(layout.markdown(block.content, { dim = true, italic = true }), {
  text = "│ ",
})

layout.separator({ label = " compacted ", dim = true })
```

Layout primitives must lower to serializable, width-independent IR. If a primitive cannot be measured from IR without calling Lua, it does not belong in this layer.

`layout.vbox`/`layout.hbox` should either reject `nil` children clearly or filter them deliberately. Default renderer examples should prefer explicit `items[#items + 1] = child` construction over `{ cond and child or nil, ... }`, because Lua sequence tables with holes are easy to truncate accidentally.

`layout.text` is wrapped plain text by default. Options should cover `hl`, `dim`, `italic`, `selectable`, and `ansi = true` for command/tool output. ANSI parsing is a Rust rendering mechanic; Lua should not pre-strip or pre-style ANSI output.

`layout.panel`/`layout.style` must support foreground/background highlight groups, padding, and optional borders so users can make tools, user messages, thinking blocks, or any other block into cards/background regions without Rust-owned chrome.

### Row caps and overflow

Capping is a general compositional primitive, not a bash/tool special case and not an option hidden inside `layout.text`.

```lua
layout.cap(child, {
  rows = 20,              -- kept content rows; marker rows are extra
  keep = "head",          -- "head" | "tail"
  marker = "below",       -- "above" | "below" | nil
})
```

`rows` is numeric. Product budgets come from `ctx.limits`:

```lua
layout.cap(child, { rows = ctx.limits.tool_body_rows, keep = "head" })
layout.cap(child, { rows = ctx.limits.tool_output_rows, keep = "tail", marker = "above" })
```

`layout.cap` stores a numeric row count in IR. Named product budgets do not belong inside the primitive. Centralized policy lives in `ctx.limits`, which Lua reads while building the width-independent layout:

```lua
ctx.limits = {
  tool_header_rows = 20, -- kept rows; marker `... N below` is extra
  tool_body_rows = 20,   -- structured bodies such as diffs/file views; no marker by default
  tool_output_rows = 20, -- raw output kept rows; marker `... N above` is extra
}
```

The key detail is that caps operate on **rendered rows after wrapping**, not raw lines. Tail caps therefore require Rust to measure the child first, then render the requested row range. Lua must not pre-split or pre-tail output by terminal width.

Bundled Lua exposes this policy helper from a defaults module, not from `smelt.layout`:

```lua
local defaults = require("smelt.transcript.defaults")
defaults.render_tool_output(output, ctx, opts?)
```

The helper is ordinary Lua composition over generic primitives, roughly:

```lua
local function render_tool_output(output, ctx, opts)
  opts = opts or {}
  local content = output and output.content or ""
  local rows = opts.rows or ctx.limits.tool_output_rows
  local hl = opts.hl or (output and output.is_error and "ErrorMsg" or nil)
  return layout.cap(
    layout.gutter(layout.text(content, { hl = hl, dim = hl == nil, ansi = true }), { text = "  " }),
    { rows = rows, keep = "tail", marker = "above" }
  )
end
```

Do not expose this as `smelt.layout.tool_output`. Keeping it in `smelt.transcript.defaults` makes it reusable by user-overridden renderers without polluting the primitive layout namespace.

So the answer to “overflow by how many lines?” is: by `ctx.limits.tool_output_rows`, currently 20 kept rendered rows plus one marker row when truncated. Individual composite renderers may pass a smaller numeric `rows` when they intentionally reserve space for nearby body content.

Plain `layout.text` remains uncapped unless composed with `layout.cap`; file views, diffs, and arbitrary custom layouts must not start tailing by accident. This fixes the current bash symptom where a command such as `cargo test ... 2>&1 | tail -120` returns the right final lines, but the transcript cap shows the first rows of that returned tail. The command string should not be special-cased; the semantic is that tool output tails when capped.

### Transcript renderer

Expose exactly one Rust-facing transcript override point:

```lua
smelt.transcript.set_renderer(function(block, ctx)
  return smelt.transcript.defaults.render(block, ctx)
end)
```

Callback shape:

```lua
function(block, ctx) -> smelt.layout | nil
```

There is no Rust-facing `set_tool_renderer`, keyed role registry, or body-only renderer registry. A tool is just another transcript block. Per-kind and per-tool dispatch are ordinary Lua code inside the root renderer or inside the bundled defaults.

The default install is equivalent to:

```lua
local transcript = require("smelt.transcript")
local defaults = require("smelt.transcript.defaults")

transcript.set_renderer(function(block, ctx)
  return defaults.render(block, ctx)
end)
```

For plugin/user cooperation, expose a Lua-only middleware helper over the same single root renderer:

```lua
smelt.transcript.get_renderer() -> function
smelt.transcript.extend_renderer(name, function(next, block, ctx)
  return next(block, ctx)
end) -> Reg
smelt.transcript.invalidate_renderer()
```

`extend_renderer` composes functions in Lua. It does not create a Rust registry by block kind, role, tool name, or tool body. Its callback receives the next renderer and either handles the block or delegates. `set_renderer`, `extend_renderer`, removing an extension registration, and `invalidate_renderer` all bump renderer generation and invalidate derived DisplayIR; width/theme/scroll do not.

```lua
local transcript = require("smelt.transcript")
local defaults = require("smelt.transcript.defaults")
local layout = require("smelt.layout")

transcript.extend_renderer("my_bash_cards", function(next, block, ctx)
  if block.kind == "tool" and block.name == "bash" then
    return layout.panel(
      next(block, ctx),
      { bg = "SmeltBashBg", padding = { x = 1 } }
    )
  end
  return next(block, ctx)
end)
```

Closed-over mutable Lua state is allowed but must be made explicit to the cache. For example, if user config toggles a local `compact` flag that changes renderer output without calling `set_renderer` or reloading Lua, it must call `smelt.transcript.invalidate_renderer()` so Rust discards DisplayIR entries compiled under the previous generation.

`defaults.render` is plain Lua dispatch:

```lua
function M.render(block, ctx)
  if block.kind == "tool" then
    return M.render_tool(block, ctx)
  elseif block.kind == "user" then
    return M.render_user(block, ctx)
  elseif block.kind == "assistant" then
    return M.render_assistant(block, ctx)
  elseif block.kind == "thinking" then
    return M.render_thinking(block, ctx)
  end
  return M.render_unknown(block, ctx)
end
```

Users customize by replacing or extending the root renderer:

```lua
transcript.set_renderer(function(block, ctx)
  if block.kind == "tool" and block.name == "bash" then
    return layout.panel(
      layout.vbox({
        my_bash_header(block, ctx),
        defaults.render_tool_output(block.output, ctx),
      }),
      { bg = "SmeltBashBg", padding = { x = 1, y = 0 } }
    )
  end

  if block.kind == "user" then
    return layout.panel(layout.markdown(block.text), { bg = "SmeltUserBg", padding = 1 })
  end

  return defaults.render(block, ctx)
end)
```

This keeps all transcript/product dispatch overrideable in Lua instead of baking precedence rules into Rust.

`ctx` is stable and width/theme-independent:

```lua
ctx = {
  show_thinking = true,
  renderer_generation = 42,
  limits = {
    tool_header_rows = 20,
    tool_body_rows = 20,
    tool_output_rows = 20,
  },
}
```

`block` is a semantic snapshot. Example tool block:

```lua
block = {
  id = 123,
  index = 7,
  kind = "tool",
  call_id = "call_abc",
  name = "bash",
  args = { ... },
  summary = styled_lines,
  status = "pending" | "confirm" | "ok" | "err" | "denied",
  elapsed = elapsed_ref, -- for layout.elapsed(block.elapsed); not part of width/theme cache churn
  elapsed_secs = 12,    -- terminal/static elapsed value when applicable
  user_message = "...",
  output = {
    content = "...",
    is_error = false,
    metadata = { ... },
  },
}
```

On `nil` or error, Rust records the error and falls back to a safe built-in default layout. A renderer that intentionally wants to hide a block must return `layout.empty()` instead of `nil`.

### Tool rendering in Lua defaults

Break the current body-only tool `render(args, output, ctx)` contract. The only public transcript override remains `smelt.transcript.set_renderer(fn)`.

The bundled default tool renderer is factored for reuse but is not privileged:

```lua
function M.render_tool(block, ctx)
  local items = {}

  items[#items + 1] = M.render_tool_header(block, ctx)

  if block.user_message then
    items[#items + 1] = layout.gutter(
      layout.text(block.user_message, { dim = true }),
      { text = "  " }
    )
  end

  if block.status ~= "denied" then
    local body = M.render_tool_body(block, ctx)
    if body then items[#items + 1] = body end
  end

  return layout.vbox(items)
end
```

Tool-specific bodies are ordinary Lua dispatch inside the defaults module, not a separate Rust API:

```lua
local tool_bodies = {
  write_file = function(block, ctx)
    if block.output and block.output.is_error then return nil end
    return layout.file_view({
      content = block.args.content or "",
      path = block.args.file_path or "",
    })
  end,
}

function M.render_tool_body(block, ctx)
  local render = tool_bodies[block.name]
  if render then
    local body = render(block, ctx)
    if body then
      return layout.cap(body, { rows = ctx.limits.tool_body_rows, keep = "head" })
    end
  end
  return M.render_tool_output(block.output, ctx)
end
```

`bash` needs no special body function in the default design; it naturally falls through to `defaults.render_tool_output(block.output, ctx)`.

Users can still replace one tool completely from the single root renderer:

```lua
transcript.set_renderer(function(block, ctx)
  if block.kind == "tool" and block.name == "bash" then
    return layout.panel(
      layout.vbox({
        my_bash_header(block, ctx),
        defaults.render_tool_output(block.output, ctx, { rows = 80 }),
      }),
      { bg = "SmeltBashBg", border = "rounded", padding = { x = 1 } }
    )
  end
  return defaults.render(block, ctx)
end)
```

Bundled default tool renderer preserves current behavior:

- status marker color:
  - `pending` -> `SmeltToolPending`
  - `ok` -> `SmeltSuccess`
  - `err` -> `ErrorMsg`
  - `denied` -> `ErrorMsg`
  - `confirm` -> `SmeltAccent`
- `*` marker and tool name;
- styled summary spans;
- elapsed suffix except confirm;
- pending title suffix spans;
- user message line;
- denied body suppression;
- fallback text/ANSI output composed from `layout.cap(layout.gutter(layout.text(...)))`;
- max tool body row cap/truncation, with raw tool output tail-capped by rendered rows and marked as `... N above`.

## Block-type requirements

### User

Current behavior to preserve:

- `SmeltUserBg` panel background;
- one-cell inner padding;
- top/bottom blank panel rows;
- trimming leading/trailing blank logical lines;
- tab expansion and control-character sanitization;
- slash command token accent;
- image labels accent;
- `@file` references accent;
- non-selectable padding/chrome;
- full-row bg fill.

Suggested default Lua:

```lua
return layout.panel(
  defaults.render_user_text(block, ctx),
  { hl = "SmeltUserBg", padding = 1 }
)
```

Default `render_user_text` should use general span/text primitives plus Rust-provided semantic annotations or a narrowly mechanical tokenizer so slash-command/image/ref behavior is not duplicated incorrectly in Lua. It is still a default helper, not block chrome hidden in Rust; users can replace it with plain markdown/text or their own styling.

Edge cases:

- copy/yank should return the original user display text, not padding;
- bg fill must be row decoration, not only literal spaces;
- command/ref/image accenting must survive wrapping;
- sanitization should stay in Rust primitives.

### Assistant text

Current behavior to preserve:

- markdown parsing and rendering;
- headings/lists/blockquotes/rules;
- fenced/indented code;
- tables with Smelt custom fitting/wrapping/borders;
- inline code/emphasis/strike/links;
- source ranges so copy/yank returns original markdown.

Suggested default Lua:

```lua
return layout.markdown(block.content)
```

Edge cases:

- markdown IR must remain width-independent;
- unsupported markdown must never drop visible text;
- source_text/copy group behavior is critical for tables and code blocks;
- syntax highlighting is render-only, not serialized.

### Thinking

Current behavior to preserve:

- when `show_thinking = false`, render folded summary based on first bold line and non-empty line count;
- when shown, dim italic text;
- thinking gutter `│ `;
- inline markdown styling;
- wrapping under block gutter.

Suggested default Lua:

```lua
if not ctx.show_thinking then
  return defaults.render_thinking_summary(block, ctx)
end
return layout.gutter(
  layout.markdown(block.content, { dim = true, italic = true }),
  { text = "│ " }
)
```

Default `render_thinking_summary` should preserve exact folded-summary semantics, using Rust-provided summary extraction or semantic summary fields if needed. It should stay a default helper so users can hide, box, expand, recolor, or replace thinking blocks entirely from the root renderer.

Edge cases:

- streaming thinking changes frequently, so renderer cache keys must avoid extra churn;
- hidden thinking height must be exact;
- summary generation must be deterministic and safe on malformed markdown.

### Code

Current behavior to preserve:

- preformatted code source;
- language metadata;
- syntax highlighting only during visible render;
- height independent of syntax;
- source/copy preservation.

Suggested default Lua:

```lua
return layout.code(block.content, { lang = block.lang })
```

Edge cases:

- no syntax tokens in serialized IR;
- width change rewraps but does not call Lua;
- tabs/control chars need Rust-side canonicalization.

### Tool

Current behavior to preserve:

- status marker and status color;
- tool name;
- summary and title suffix styling (`title_suffix` spans visible only while pending);
- elapsed suffix;
- user message line;
- confirm/pending/ok/err/denied distinctions;
- denied body suppression;
- fallback output rendering;
- custom body layouts;
- output/error styling;
- body cap/truncation;
- args/output/metadata availability.

Suggested default Lua:

```lua
local status_hl = {
  pending = "SmeltToolPending",
  ok = "SmeltSuccess",
  err = "ErrorMsg",
  denied = "ErrorMsg",
  confirm = "SmeltAccent",
}

local items = {
  defaults.render_tool_header(block, ctx, { marker_hl = status_hl[block.status] }),
}
if block.user_message then
  items[#items + 1] = layout.gutter(layout.text(block.user_message, { dim = true }), { text = "  " })
end
if block.status ~= "denied" then
  local body = tool_body_or_fallback(block, ctx)
  if body then items[#items + 1] = body end
end
return layout.vbox(items)
```

Edge cases:

- built-in tool render behavior to preserve before simplifying:
  - `bash`, `web_search`, `read_process_output`, and `stop_process` rely on default raw-output fallback instead of custom raw-output renderers;
  - `web_fetch` renders the prompt above raw output, so it must explicitly budget rows for the prompt and still tail the output region using the same local output helper;
  - `read_file`, `glob`, and successful `grep` render compact count summaries, while their error paths fall through to default raw-output rendering;
  - `write_file` renders `file_view`; `edit_file` and replace-mode `edit_notebook` render diffs; insert-mode `edit_notebook` renders `file_view` plus optional title text;
  - tools with no custom renderer use safe fallback output rendering.
- pending elapsed is placed by `defaults.render_tool_header` via a dynamic primitive such as `layout.elapsed(block.elapsed)`; Rust updates the rendered value without rerunning Lua every second and without putting live pending elapsed into the DisplayIR cache key;
- terminal elapsed, status, output, user message, args, and renderer generation must invalidate tool DisplayIR;
- output can be huge; use `layout.cap` plus node-level `render_range`, not eager buffers;
- raw tool output must use tail overflow semantics by rendered rows, not raw lines, so commands that already pipe through `tail` still show their final rows in the capped transcript;
- confirm/denied semantics must remain explicit and tested;
- fallback raw output must handle ANSI/control bytes safely.

### Exec

Current behavior to preserve:

- user-style panel for command;
- `!` prefix styled as `SmeltExecPrefix`;
- command sanitization;
- output below command;
- wrapped output.

Suggested default Lua:

```lua
local items = {
  layout.panel(layout.runs({
    { text = "!", hl = "SmeltExecPrefix" },
    { text = block.command },
  }), { hl = "SmeltUserBg", padding = 1 }),
}
if block.output ~= "" then
  items[#items + 1] = layout.cap(
    layout.gutter(layout.text(block.output, { dim = true, ansi = true }), { text = "  " }),
    { rows = ctx.limits.tool_output_rows, keep = "tail", marker = "above" }
  )
end
return layout.vbox(items)
```

Edge cases:

- copy should include command plus output, not panel padding;
- output may contain ANSI/control bytes;
- command/control sanitization stays in Rust primitives.

### Mode

Current behavior to preserve:

- icon styled by dynamic `hl_group`;
- mode text italic with same foreground and no bg;
- currently one display row.

Suggested default Lua:

```lua
return layout.line({
  { text = block.icon, hl = block.hl_group },
  { text = block.text, hl = block.hl_group, italic = true },
})
```

Add `layout.line` or a non-wrapping/clipped variant if we want to preserve one-row behavior exactly.

Edge cases:

- decide explicitly whether long mode text clips or wraps;
- dynamic highlight group names must resolve safely.

### Process status

Current behavior to preserve:

- `SmeltProcess`;
- italic;
- no background;
- wrapping.

Suggested default Lua:

```lua
return layout.text(block.text, { hl = "SmeltProcess", italic = true })
```

Edge cases:

- process-status notes may replace the previous note; cache invalidation must follow block rewrite/generation;
- should stay lightweight and not use panel chrome.

### Compacted

Current behavior to preserve:

- centered separator label ` compacted `;
- summary below;
- dim styling;
- summary markdown;
- copy/source behavior for summary.

Suggested default Lua:

```lua
return layout.vbox({
  layout.separator({ label = " compacted ", dim = true }),
  layout.markdown(block.summary, { dim = true }),
})
```

Edge cases:

- separator is width-dependent and should be Rust primitive;
- separator should be non-selectable or copy-as empty;
- summary copy/yank source should preserve markdown.

## DisplayIR and cache keys

Renderer-produced IR must be cached by a stable key:

```text
block_id
semantic_block_hash
semantic_sidecar_hash
block_kind
renderer_generation
layout_primitive_version
```

Width and theme must not be part of the DisplayIR key.

Do not hash cached presentation back into canonical state. Today `ToolState.body` is part of the sidecar and `ToolState::display_hash()` can include the cached body; the final design should remove that loop. The semantic sidecar hash should include only canonical tool state: status, terminal elapsed, output content/error/metadata, user message, and args/summary inputs as needed. DisplayIR lives in `DisplayModel` / `session.ir.bin`, not `BlockHistory` or `ToolState`.

Width-specific row indexes remain separate and can be cached as today. Theme-dependent rendered rows and syntax tokens remain ephemeral render caches.

### Incrementality and persistence

The DisplayIR cache is derived, incremental display state. It should be built per block on cache miss or semantic/display-policy invalidation, not rebuilt for the whole transcript on every render or save.

Current code already has the important shape:

- session restore reads a disposable sidecar cache from `session.ir.bin` before rebuilding the transcript view;
- projection compiles missing/stale display entries for the requested block range rather than eagerly rendering all history;
- saving writes the current cache snapshot alongside the session and skips unchanged writes by generation/fingerprint.

The Lua-layout architecture should preserve and generalize that behavior:

- new blocks or changed semantic sidecars invalidate only their own DisplayIR entries;
- renderer generation/layout primitive version bumps invalidate affected derived entries and the row indexes built from them;
- mutable closed-over Lua state that affects renderer output must call `smelt.transcript.invalidate_renderer()` unless it changes via `set_renderer`, `extend_renderer`, extension removal, or Lua reload;
- width changes rebuild/reuse width-specific row indexes from cached IR but do not call Lua;
- theme changes resolve highlight groups/render spans again but do not call Lua and do not invalidate width-independent IR;
- scrolling renders visible row ranges from existing IR and exact measurements;
- save persists the display cache snapshot that already exists in memory; save must not force a full transcript recompute;
- deleting `session.ir.bin` is always safe and changes only warm-start performance, never transcript semantics.

The persisted cache should remain a sidecar optimization, not canonical session data. If cache decoding, validation, renderer generation, or version checks fail, Rust should discard the sidecar and rebuild derived entries lazily from canonical history.

Node rendering contract:

```text
measure(node, width) -> exact_rows
render_range(node, width, row_start..row_end) -> rows + metadata
```

Every primitive, including `vbox`, `hbox`, `cap`, `panel`, `gutter`, markdown, source views, and dynamic elapsed text, must satisfy `measure == full render row_count`. `cap(keep="tail")` is implemented by measuring the child, computing the kept child row range, rendering only that range, then inserting the marker row. `hbox` must not render all off-screen child rows into scratch buffers.

On renderer policy changes:

- `set_renderer`, `extend_renderer`, extension removal, `invalidate_renderer`, and Lua reload bump renderer generation;
- invalidate DisplayIR entries whose renderer generation no longer matches;
- invalidate row indexes derived from those entries;
- do not preserve compatibility shims.

## Failure and fallback policy

If a Lua renderer errors or returns invalid data:

1. record the error with block kind/id/renderer name;
2. throttle repeated identical errors;
3. use a Rust minimal fallback layout for that block;
4. keep the transcript usable and measurable.

Fallbacks are not compatibility renderers. They are crash-safety renderers.

## Testing strategy

Use two distinct kinds of coverage. Do not let one stand in for the other.

### Primitive/property tests

These prove the layout engine is exact independent of current product styling. For every layout primitive and block renderer:

```text
measure(width) == render(width, full_range).row_count
```

Run this as property-style coverage across widths including degenerate widths 0/1.

Required primitive coverage:

- `layout.cap` head and tail modes with wrapped lines, wide glyphs, ANSI spans, marker rows, and exact marker counts;
- default raw-output helper preserving dim success styling, `ErrorMsg` error styling, ANSI stripping/styling, gutter, copy/selectability, and tail row selection;
- `hbox` measuring/rendering only requested visible ranges;
- dialog preview vs transcript rendering using the same IR with different explicit chrome.

### Storybook product snapshots

These prove the bundled Lua defaults preserve user-visible behavior. Before migrating each block type, add/lock snapshots using the existing storybook (`crates/tui/tests/storybook/stories`) and add missing stories before changing code. Required coverage:

- tool pending/confirm/ok/err/denied status marker colors;
- bash/tool output error styling;
- denied tool body suppression;
- elapsed suffix;
- pending title suffix spans;
- tool user message line;
- fallback raw output;
- long bash/tool output shows the rendered tail with `... N above`, including commands whose captured content already came from shell `tail`;
- composite tool output such as `web_fetch` reserves any prompt/header rows explicitly and still tails the actual output region;
- user panel bg/padding/fill;
- user slash command accent;
- user image labels and `@file` references;
- exec `!` prefix style and output;
- thinking expanded and collapsed;
- process status italic/style;
- mode icon/text style;
- compacted separator/summary;
- assistant markdown tables/code/links/copy source;
- copy/yank/search/selectability for Lua-rendered chrome;
- a custom root renderer extending/replacing tool and user blocks with different panel backgrounds, proving headers/chrome are not Rust-owned.

## Migration phases

### Phase 1 — Define general LayoutIR primitives

Goal: create the Rust IR that Lua renderers will return.

Work:

- Rename/generalize current tool-only `BlockLayout` / `ToolBody` into display names (`LayoutIr`, `LayoutNode`, `LayoutLeaf`). Keep the `ToolBody` bridge only until the body-only tool contract is deleted.
- Rename source-view internals so `file_view` is not stored as `IrLeaf::DiffIr`; use `SourceViewIr` or equivalent.
- Remove `layout.leaf(buf)` from the primary API.
- Add the first generic primitive slice needed by current tool bodies and previews: `text`, `diff`, `file_view`, `vbox`, `hbox`, `gutter`, `cap`, and `empty`. Do not expose a fake `markdown` primitive; add real markdown IR with the full-block renderer migration. Add the broader full-block primitives (`runs`, `line`, `elapsed`, `code`, `panel`, `separator`, `spacer`, `source`, `selectable`, `copy_as`, `style`) with the block renderer migration that first consumes them, so their row metadata contracts are designed against real snapshots instead of placeholders.
- Add generic capping semantics (`head`/`tail`, numeric row budget, marker direction) implemented in Rust. Keep raw tool-output rendering as bundled Lua composition over generic primitives, not as `smelt.layout.tool_output`.
- Implement `hbox` with node-level measurement and row-range rendering from the start. It must not render full off-screen child columns; the current one-row bridge buffer is acceptable only as a short-lived compositor debt until span-level row compositing lands.
- Ensure every Phase-1 primitive is serializable, width-independent, and theme-independent.
- Implement `measure(width)` and `render_range(width, rows)` for Phase-1 primitives.
- Add primitive tests for measure/render equality where behavior changed, especially capping.
- Add or update storybook/unit cases for long raw output tailing and structured output row caps before changing behavior.

Exit criteria:

- existing tool bodies can be expressed without buffer leaves;
- width changes do not invalidate primitive IR;
- no direct Lua buffer leaves remain in the transcript/tool display path.

Status after implementation:

- `smelt.layout.leaf`, `smelt.layout.tool_output`, and the Lua bootstrap buffer-backed layout helpers are removed from the primary API, generated docs, and Lua stubs.
- `LayoutLeaf`, `LayoutIr`, `LayoutNode`, and `SourceViewIr` are in `crates/core/src/content/block_layout.rs`; `IrLeaf::DiffIr` is gone.
- `layout.empty`, `layout.gutter`, and `layout.cap` are registered in `crates/core/src/lua/api/layout.rs`; raw output policy lives in `runtime/lua/smelt/transcript/defaults.lua`. `layout.markdown` is intentionally absent until it can lower to real markdown IR instead of pretending to be text.
- Tool output helpers now tail-cap rendered rows with an `above` marker by default, fixing the capped-head-of-tail output bug.
- The renderer implements exact node measurement and requested-row rendering for the Phase-1 primitives, including caps and hboxes. `layout.gutter` now reduces child wrap width and replaces any inherited fallback gutter with its explicit prefix during measurement and rendering. Hbox no longer renders full child columns; it uses per-visible-row compositing until a lower-level span compositor replaces the one-row bridge buffer.
- Coverage includes the structured source-view row cap and raw-output tail cap unit paths; broader full-block primitive coverage moves with the root renderer migration.

### Phase 2 — Separate semantic state from derived display cache

Goal: fix cache ownership before changing renderer contracts.

Work:

- Remove renderer-produced layout from canonical `ToolState` / `BlockHistory` state.
- Split semantic tool-state hashing from derived DisplayIR caching: status, terminal elapsed, output content/error/metadata, user message, args, and summary inputs are semantic; cached layout is not.
- Store renderer-produced layouts in `DisplayModel` and in `session.ir.bin` derived cache entries keyed by semantic block/sidecar hashes plus renderer/layout versions.
- Hydrate derived layouts into the display model, then hydrate/validate row indexes against those derived entries.
- Keep cache deletion safe: removing `session.ir.bin` must never change transcript semantics.

Exit criteria:

- `ToolState.display_hash()` or its replacement cannot include cached layout.
- Row-index hydration still validates against current semantic history.
- Width/theme changes and cache hydration do not mutate canonical transcript state.

Status after implementation:

- `ToolState` now contains only semantic fields: status, elapsed, output, and user message. `BlockHistory` no longer has `install_tool_body`, `hydrate_tool_body_cache`, or body-cache mutation helpers.
- `DisplayModel` owns derived tool bodies, serializes them as `ToolBodyCacheEntry` in `session.ir.bin`, and compiles `DisplayBlock::ToolCall` with an optional cached body without changing canonical history.
- Tool-body cache hydration validates block id, call id, content hash, semantic sidecar hash, and display renderer version before installing into `DisplayModel`; deleting `session.ir.bin` falls back to semantic output without changing transcript state.
- Installing a newly rendered tool body invalidates compiled display rows, exact row indexes, materialized rows, and persisted row-index entries, then replans with the derived body. Hydration and width/theme changes do not bump `BlockHistory` generation.
- Generated Lua docs now derive the `smelt.ui.layout.Measure` type from Rust metadata instead of hand-patching generated stubs.

Deferred debt:

- `hbox` still uses a one-row scratch buffer for visible child-row span clipping. It no longer renders off-screen child columns, but a real row-span compositor should replace the bridge when hbox grows beyond current tool/preview usage.
- Rust's emergency raw-output fallback intentionally mirrors the Lua default helper so renderer failure remains safe. Phase 4 should delete the body-only fallback path when full-block Lua tool rendering owns all tool chrome.

### Phase 3 — Add root transcript renderer and default Lua renderers

Goal: introduce Lua as the source of display policy without changing visible output.

Work:

- Add `smelt.transcript.set_renderer(fn)` as the single Rust-facing transcript override point.
- Add Lua-level `smelt.transcript.get_renderer()`, `smelt.transcript.extend_renderer(name, fn)`, and `smelt.transcript.invalidate_renderer()` for cooperative middleware composition and explicit invalidation of closed-over renderer state, without Rust per-kind/tool registries.
- Add semantic block snapshots passed to the root callback.
- Implement bundled default renderers for every block type, using explicit item-list construction instead of nil-holed Lua arrays.
- Install the bundled defaults through the same root renderer API; do not add keyed role or tool renderer registries in Rust.
- Implement Rust fallback layouts for renderer failures.
- Add renderer generation and invalidation hooks on reload.

Exit criteria:

- all block types have bundled Lua defaults reachable through `defaults.render(block, ctx)`;
- user code can replace or extend the single root renderer and still call bundled defaults;
- renderer errors do not break transcript rendering;
- default Lua renderers produce current snapshots for simple block cases.

Status after implementation:

- `smelt.transcript` is now a host-tier namespace with one Rust-facing root renderer handle, plus Lua wrapper helpers in `runtime/lua/smelt/transcript.lua`: `set_renderer`, `get_renderer`, `extend_renderer`, and `invalidate_renderer`.
- Renderer generation lives in `LuaShared`; `set_renderer`, `extend_renderer`, extension removal, explicit invalidation, and reload all bump it. Transcript projections and resume previews observe that generation and clear derived display caches when it changes. Width/theme/scroll changes do not bump it.
- The bundled default root renderer is installed during bootstrap through the same public `set_renderer` API and calls `smelt.transcript.defaults.render(block, ctx)`.
- Rust can now invoke the root renderer with semantic block snapshots and a width/theme-independent context. Errors, `nil`, invalid return types, or missing renderers record an error and fall back to minimal Rust layout; `layout.empty()` remains the explicit zero-row/hide result.
- `smelt.transcript.defaults` has default functions for every current block kind and keeps the raw tool-output helper as ordinary Lua composition over `layout.text`, `layout.gutter`, and `layout.cap`.
- Unit coverage exercises default simple-block rendering, middleware composition/removal/generation behavior, explicit invalidation, renderer error fallback, `nil` fallback, and `layout.empty()` hiding.

Deferred debt:

- The live transcript projection still uses the existing Rust block renderers until Phase 4/5 migrate tool and non-tool blocks to root-renderer-produced LayoutIR. This preserves visible output while the root API and default Lua policy become available.
- Because the current primitive slice lacks markdown, panel, runs/line, code, separator, style, source/copy/selectable, and elapsed primitives, Phase 3 defaults are fallback-safe and structurally complete but intentionally do not attempt to reproduce every product visual. The snapshot-preserving migrations land with the corresponding primitives in Phase 4/5.

### Phase 4 — Move tool rendering to full-block Lua layout

Goal: delete body-only tool render semantics and move tool chrome policy to bundled Lua.

Work:

- Delete the old body-only `render(args, output, ctx)` contract.
- Move current tool title/status/body fallback behavior into bundled Lua defaults.
- Keep built-in tool-specific body renderers as ordinary Lua dispatch inside `defaults.render_tool_body`, not as a separate Rust registry or `set_tool_renderer` API.
- Preserve all status colors and state-specific behavior, including pending elapsed updates.
- Update built-in tools to declarative layout primitives; raw-output-only tools should rely on the default renderer fallback.
- Delete Rust-backed `smelt.layout.tool_output` and do not replace it in `smelt.layout`.
- Do not add `layout.tool_header`; default tool headers are Lua helper composition over lower-level primitives such as `layout.runs`, `layout.gutter`, `layout.cap`, and `layout.elapsed`.
- Remove old body-only compatibility code.

Exit criteria:

- tool snapshots match current behavior;
- no Rust hardcoded `* name elapsed` tool title policy remains;
- tool IR cache is width-independent;
- denied/confirm/pending/ok/err behavior is covered by tests.

### Phase 5 — Route non-tool blocks through Lua defaults

Goal: remove hardcoded Rust per-block display policy.

Suggested order:

1. process_status, mode, compacted;
2. exec;
3. user;
4. thinking;
5. assistant text/code.

Work:

- For each block type, lock snapshots first.
- Switch compile path to call Lua renderer and store LayoutIR.
- Delete the old hardcoded renderer once the Lua default is proven.
- Keep only Rust primitive implementations.

Exit criteria:

- no duplicate old/new renderer path for migrated blocks;
- snapshots match or intentional changes are documented;
- copy/search/selectability tests pass.

### Phase 6 — Persist renderer-produced DisplayIR

Goal: make cold resume/preview scale with large sessions.

Work:

- Extend `session.ir.bin` to store general DisplayIR, not only tool bodies and row indexes.
- Include renderer generation and layout primitive version in cache keys; do not hash Lua closures or pretend to detect hidden closed-over state automatically.
- Key DisplayIR from semantic block/sidecar hashes only; never include cached DisplayIR in the hash input.
- Hydrate DisplayIR before first projection.
- Reject stale cache entries cleanly on renderer reload/version changes.

Exit criteria:

- width/theme changes do not call Lua;
- large-session resume hydrates existing IR and measures cheaply;
- cache is disposable and safe to rebuild.

### Phase 7 — Remove obsolete Rust renderer modules and old APIs

Goal: consolidate around the new architecture.

Work:

- Delete tool-body-only APIs.
- Delete old per-block Rust renderer modules that are now policy-only.
- Keep Rust modules only for primitive mechanics: markdown/code/diff/text wrapping/panel/gutter/etc.
- Update docs and Lua stubs.
- Regenerate Lua API docs.

Exit criteria:

- one display path;
- one layout IR;
- no buffer-leaf transcript API;
- no compatibility shims;
- code reads as if Lua-defined transcript layout was always the design.

### Phase 8 — Performance and regression validation

Goal: prove the rewrite achieves the original transcript layout goals.

Work:

- Run the large mixed transcript baseline.
- Add counters for Lua renderer calls during:
  - first projection;
  - width resize;
  - theme swap;
  - scroll;
  - resume preview.
- Assert width/theme/scroll with valid IR does not call Lua.
- Compare first/resume/resize latencies against the plan baseline.

Exit criteria:

- `cargo fmt && cargo clippy --workspace --all-targets -- -D warnings` passes;
- `cargo nextest run --workspace` passes;
- large baseline improves by avoiding full-block render measurement;
- no current storybook/UI snapshots regress unintentionally.

## Resolved design choices

- Expose one Rust-facing root override point: `smelt.transcript.set_renderer(fn)`.
- Provide Lua-level root-renderer composition with `smelt.transcript.get_renderer()` and `smelt.transcript.extend_renderer(name, fn)`. This is middleware around the single root renderer, not a Rust per-kind/per-tool registry.
- Provide `smelt.transcript.invalidate_renderer()` for renderer-affecting closed-over Lua state changes that do not go through `set_renderer`, `extend_renderer`, extension removal, or Lua reload.
- Transcript/default/preview renderers return declarative LayoutIR only. Imperative buffer/render/paint APIs remain low-level escape hatches for windows, overlays, prompt/status bars, pickers, and debug panels; they are not transcript display APIs.
- Do not add `smelt.transcript.renderer.set(kind, fn)`, `set_tool_renderer`, or a Rust-owned tool/body renderer registry.
- Bundle default renderers as normal Lua modules. The default root renderer calls `smelt.transcript.defaults.render(block, ctx)`; users can call, compose, copy, or ignore those helpers.
- Keep per-block and per-tool dispatch in Lua. Built-in tool body functions may live in a Lua table inside the defaults module, but that table is implementation/composition code, not a Rust API.
- Do not add `layout.tool_header(block)`. Tool headers are default Lua composition, exposed as `smelt.transcript.defaults.render_tool_header(block, ctx, opts?)` for reuse.
- Keep pending elapsed render-dynamic through a lower-level primitive such as `layout.elapsed(block.elapsed)`, so Lua decides placement/chrome while Rust updates the displayed value without rerunning Lua.
- Preserve current mode one-row behavior with `layout.line` unless deliberately changed with snapshots.
- Keep exact user-text highlighting via general span/text primitives plus Rust-provided semantic annotations or narrowly mechanical tokenization; do not hide user-message panel chrome in Rust.
- Implement tail/head capping as generic `layout.cap` IR with numeric row counts from `ctx.limits`; do not keep `smelt.layout.tool_output`.
- Do not special-case command strings containing `tail`; the row-selection semantics belong to the default raw-output composition over `layout.cap`.

## Remaining open design decisions

1. Should mode blocks always clip to one row exactly, or should `layout.line` offer configurable clip/ellipsis behavior?
2. What exact shape should Rust-provided user-text annotations take: precomputed spans on `block`, a general tokenizer primitive, or a helper in defaults that calls a mechanical tokenizer?
3. Should `extend_renderer` support priority/load-order options beyond “later extensions run first,” or is named registration/removal enough for the first implementation?
