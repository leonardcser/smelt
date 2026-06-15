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

- TUI transcript caching now stores renderer-produced `LayoutIr` directly; the old `DisplayBlock::{User, Text, ToolCall, ...}` enum dispatch and single-variant wrapper are gone.
- Renderer-produced tool layouts are no longer stored on canonical `ToolState` / `BlockHistory`. Semantic tool state remains status, terminal elapsed, output, and user message; tool calls compile full-block `LayoutIr` through the root Lua transcript renderer.
- The body-only `ToolBody` bridge and tool `render(args, output, ctx)` contract have been deleted. `BlockLayout` now has display-oriented aliases (`LayoutIr`, `LayoutNode`, `LayoutLeaf`) and compiled source leaves use `SourceViewIr` instead of `IrLeaf::DiffIr`.
- Tool chrome policy (`* name summary elapsed`, status color, denied/user-message behavior, and built-in tool bodies) now lives in bundled Lua defaults. Rust keeps only primitive mechanics plus emergency fallback layouts for renderer failure.
- Tool preview and transcript body currently share `BlockLayout` values but differ through implicit Rust render context such as the tool-block base gutter. Replace this with explicit Lua chrome/gutter composition when full-block tool rendering lands.
- `smelt.layout.tool_output` has been deleted from the `smelt.layout` API. Raw tool output is now bundled Lua composition in `smelt.transcript.defaults.render_tool_output`, implemented as `layout.cap(layout.gutter(layout.text(..., { ansi = true })), { keep = "tail", marker = "above" })`.
- Compiled source-view leaves now use `SourceViewIr`. File views still reuse the diff/file-view renderer mechanics internally, but the public/display IR no longer stores them as `IrLeaf::DiffIr`.
- `render_ir_hbox` now measures child nodes directly and renders requested row ranges instead of rendering full child columns. The current bridge still uses a one-row temporary buffer per visible child row to preserve existing span clipping until row compositing is generalized.
- `layout.markdown` is not currently exposed as a text-backed placeholder. It should return real markdown LayoutIR when the full-block renderer migration needs it; until then bundled Lua uses `layout.text` for plain fallback bodies.
- Unit coverage now includes the motivating raw-output tail cap. Storybook still lacks a dedicated long-output tail case; add one before broader full-block renderer changes if visual coverage is needed.

## Design pressure points to settle before implementation

These are the areas most likely to create accidental complexity or hidden Rust policy if left vague:

1. **Root renderer composability.** One public `set_renderer(fn, opts?)` keeps Rust simple, but “last setter wins” can be hostile to plugins. Provide a small Lua-only middleware helper around the root renderer:
   `transcript.get_renderer()` / `transcript.set_renderer(fn, opts?)` / `transcript.extend_renderer(name, fn, opts?)`. This must not become a Rust-owned per-kind or per-tool registry.
2. **Intentional empty blocks.** `nil` should mean invalid renderer output/fallback, not “hide this block,” or user custom renderers cannot intentionally suppress a block. Add an explicit `layout.empty()` or equivalent zero-row node if hiding/collapsing is supported.
3. **Determinism and cache invalidation.** A root renderer can close over arbitrary Lua globals. The contract is that renderer output must be deterministic for `(block, ctx, renderer generation)` at runtime, and for `(block, ctx, opts.cache_key)` across process restarts when persisted DisplayIR is enabled. `set_renderer`, `extend_renderer`, extension removal, and Lua reload bump generation; width/theme/scroll do not. Do not try to hash Lua closures. If user/plugin code mutates renderer-affecting closed-over state without reinstalling/reloading the renderer, it must call an explicit invalidation API that bumps renderer generation and disables persistence until the renderer chain is reinstalled with stable cache keys.
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
layout.cap(child, { rows = ctx.limits.tool_output_rows, keep = "tail", marker = "above" })
```

`layout.cap` stores a numeric row count in IR. Named product budgets do not belong inside the primitive. Centralized policy lives in `ctx.limits`, which Lua reads while building the width-independent layout:

```lua
ctx.limits = {
  tool_header_rows = 20, -- kept rows; marker `... N below` is extra
  tool_body_rows = 20,   -- legacy/default budget available to custom renderers
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
  return layout.gutter(
    layout.cap(
      layout.text(content, { hl = hl, dim = hl == nil, ansi = true }),
      { rows = rows, keep = "tail", marker = "above" }
    ),
    { text = "  " }
  )
end
```

Do not expose this as `smelt.layout.tool_output`. Keeping it in `smelt.transcript.defaults` makes it reusable by user-overridden renderers without polluting the primitive layout namespace.

So the answer to “overflow by how many lines?” is: by `ctx.limits.tool_output_rows`, currently 20 kept rendered rows plus one marker row when truncated. Composite renderers should call `render_tool_output` or add their own explicit `layout.cap` only for raw output that can explode.

Plain `layout.text` remains uncapped unless composed with `layout.cap`; file views, diffs, plan summaries, and arbitrary structured custom layouts must not start tailing by accident. This fixes the bash/read-process symptom where a command such as `cargo test ... 2>&1 | tail -120` returns the right final lines, but the transcript cap shows the first rows of that returned tail. The command string should not be special-cased; the semantic is that raw process output tails when capped.

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

`extend_renderer` composes functions in Lua. It does not create a Rust registry by block kind, role, tool name, or tool body. Its callback receives the next renderer and either handles the block or delegates. `set_renderer`, `extend_renderer`, removing an extension registration, and `invalidate_renderer` all bump renderer generation and invalidate derived DisplayIR; width/theme/scroll do not. `opts.cache_key` on `set_renderer` and `extend_renderer` is the opt-in contract for persisted DisplayIR across process restarts.

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
- Display cache persistence now stores disposable row-index entries only. Tool layouts are derived by compiling full-block Lua transcript layout and are invalidated through display keys rather than persisted as body-cache records.
- Renderer generation and semantic layout keys invalidate compiled display rows, exact row indexes, materialized rows, and persisted row-index entries. Hydration and width/theme changes do not bump `BlockHistory` generation.
- Generated Lua docs now derive the `smelt.ui.layout.Measure` type from Rust metadata instead of hand-patching generated stubs.

Deferred debt:

- `hbox` still uses a one-row scratch buffer for visible child-row span clipping. It no longer renders off-screen child columns, but a real row-span compositor should replace the bridge when hbox grows beyond current tool/preview usage.
- Rust's emergency fallback now mirrors the Lua default at a minimal level so renderer failure remains safe; it is not a compatibility path for tool-specific rendering.
- The earlier non-tool/default-primitive migration debt was completed in Phases 5-7. Remaining primitive/API cleanup is tracked with the Phase 8 follow-up candidates below, especially generic styling and dynamic elapsed text.

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

- `smelt.transcript` is now a host-tier namespace with one Rust-facing root renderer handle, plus Lua wrapper helpers in `runtime/lua/smelt/transcript.lua`: `set_renderer`, `get_renderer`, `extend_renderer`, and `invalidate_renderer`. The host-tier transcript bootstrap now runs in core/headless runtimes; TUI bootstraps layer UiHost modules after replacing the `smelt` table with the full API surface.
- Renderer generation lives in `LuaShared`; `set_renderer`, `extend_renderer`, extension removal, explicit invalidation, and reload all bump it. Transcript projections and resume previews observe that generation and clear derived display caches when it changes. Width/theme/scroll changes do not bump it.
- The bundled default root renderer is installed during bootstrap through the same public `set_renderer` API and calls `smelt.transcript.defaults.render(block, ctx)`.
- Rust can now invoke the root renderer with semantic block snapshots and a width/theme-independent context. Errors, `nil`, invalid return types, or missing renderers record an error and fall back to minimal Rust layout; `layout.empty()` remains the explicit zero-row/hide result.
- Shared semantic annotations (`elapsed_text`, `status_hl`, `thinking_summary`) are computed in Rust and passed to Lua defaults so the emergency fallback and bundled policy do not maintain separate formatting rules.
- `smelt.transcript.defaults` has default functions for every current block kind and keeps the raw tool-output helper as ordinary Lua composition over `layout.text`, `layout.gutter`, and `layout.cap`.
- Lua layout compilation now lives with the display-layout layer instead of app transcript plumbing, so tool-body and root-rendered layouts share one conversion seam.
- Generated API docs now mark mixed Host/UiHost namespaces and per-function tiers, avoiding the earlier flattening of `smelt.transcript` inspection APIs into the Host label.
- Unit coverage exercises host-runtime transcript bootstrap, default simple-block rendering, middleware composition/removal/generation behavior, explicit invalidation, renderer error fallback, `nil` fallback, and `layout.empty()` hiding.

Deferred debt:

- Superseded by Phase 5: live transcript projection now routes every block kind through the root Lua renderer and stores renderer-produced `LayoutIr`.
- Superseded by Phase 5: markdown, panel, line, code, and separator primitives exist and are consumed by defaults. Generic `layout.style`, copy/source/selectability wrappers, and dynamic `layout.elapsed` remain follow-up candidates after Phase 8 performance validation.

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

Status after implementation:

- The old tool `render(args, output, ctx)` hook is removed from `smelt.tools.ToolDef`, `ToolHandles`, docs, generated stubs, and all bundled tool registrations.
- Tool calls now compile full-block `LayoutIr` by invoking the root transcript renderer. Rust no longer pre-renders or caches a body-only `ToolBody`, and the old `DisplayBlock::ToolCall` branch is gone.
- `runtime/lua/smelt/transcript/defaults.lua` owns the default tool block: styled `layout.runs` header, status colors, elapsed/title suffix placement, user messages, denied body suppression, and raw-output tail caps. Built-in structured body functions live next to their tools and populate the defaults' private dispatch table.
- `layout.runs` preserves styled summary spans, syntax highlighting, selectability, and `title_suffix` metadata so Lua-rendered headers keep the old copy/yank behavior. Tool summaries and `layout.runs` now share the same Rust styled-lines decoder.
- The display cache was simplified to disposable row-index entries only; derived tool bodies are no longer serialized in `session.ir.bin`. Compiled display-layout keys include the renderer context bit used by Lua so toggling `show_thinking` cannot reuse stale Lua-produced tool layout.
- Confirm previews render generic compiled layout IR, sharing the same primitive renderer path as transcript tool layouts. The generic `LayoutIr` renderer now lives under a layout-named module, with legacy wrapped-output helpers split out for exec output.

Deferred debt:

- Pending elapsed still invalidates/recompiles the Lua-produced tool layout with semantic state. Keep this simple path until Phase 8 results show whether a lower-level dynamic `layout.elapsed` primitive is worth implementing.
- `hbox` still uses the one-row scratch-buffer compositor debt from Phase 1.

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

Status after implementation:

- Transcript display now compiles one `LayoutIr` for every block kind. The compile path invokes the root Lua transcript renderer for tools and non-tools alike, then Rust only validates, measures, and renders the resulting IR.
- Added the full-block primitives consumed by defaults: `layout.line`, `layout.markdown`, `layout.code`, `layout.separator`, and `layout.panel`. `layout.gutter` now has an explicit `styled` option so Lua chooses whether row styling includes the prefix.
- Bundled Lua defaults now render user, assistant, thinking, exec, mode, process-status, compacted, and code blocks with generic primitives. Rust-provided semantic annotations expose user styled lines, exec command spans, and folded thinking summaries without hiding block chrome in Rust.
- The TUI renderer tree now keeps only primitive mechanics (`layout_ir` plus markdown internals). Obsolete per-block renderer modules for user/thinking/exec/mode/process-status/compacted/text/tool-output chrome were deleted.
- Existing storybook snapshots are preserved, including unstyled raw tool-output gutters and styled thinking/exec gutters, via explicit Lua/default gutter composition.

Follow-up cleanup:

- Display-safe control-character sanitization is centralized in `smelt_core::content`; the TUI content module re-exports the shared helpers instead of carrying a second implementation.

Deferred architecture debt:

- Generic styling is the next layout-API cleanup candidate after Phase 8. Add a composable `layout.style(child, opts)` / `StyleSpec` wrapper before adding more per-primitive style fields, then migrate default Lua renderers away from product-shaped flags where practical.
- `GutterSpec.styled` preserves current snapshot parity between raw tool-output gutters and styled thinking/exec gutters, but it is a product-shaped flag. Fold this into explicit style/gutter composition when `layout.style` lands, including defined composition for nested gutters instead of the current replacement semantics.
- Markdown/code partial rendering, panel child rows, and hbox columns still use temporary buffers in places to preserve spans while rendering requested rows. Replace those bridges with a row-span compositor/direct row-range renderer before expanding primitive complexity.
- Copy/source/selectability wrappers (`layout.source`, `layout.copy_as`, `layout.selectable`) are still absent as generic layout nodes. Add them only with tests that lock copy/yank/search behavior through panels, gutters, caps, and hboxes.

Validation after implementation:

- `cargo build`
- `cargo nextest run --workspace`
- `cargo fmt && cargo clippy --workspace --all-targets -- -D warnings`
- `cargo xtask gen-lua-docs`
- `git diff --check`

### Phase 6 — Persist renderer-produced DisplayIR

Goal: make cold resume/preview scale with large sessions.

Work:

- Extend `session.ir.bin` to store general DisplayIR alongside row indexes.
- Include renderer generation and layout primitive version in cache keys; do not hash Lua closures or pretend to detect hidden closed-over state automatically.
- Key DisplayIR from semantic block/sidecar hashes only; never include cached DisplayIR in the hash input.
- Hydrate DisplayIR before first projection.
- Reject stale cache entries cleanly on renderer reload/version changes.

Exit criteria:

- width/theme changes do not call Lua;
- large-session resume hydrates existing IR and measures cheaply;
- cache is disposable and safe to rebuild.

Status after implementation:

- `session.ir.bin` now serializes `DisplayCacheData { row_indexes, display_layouts }`; cache read/write and background persist metrics report both entry types. The cache payload schema is separated from renderer semantics by `FORMAT_VERSION`, while renderer-produced layout semantics remain gated by `DISPLAY_RENDERER_VERSION`. The current file format stores row indexes and DisplayIR in independently decoded payload sections so a corrupt/stale DisplayIR payload does not discard an otherwise valid exact row index.
- `DisplayLayoutCacheEntry` persists renderer-produced `LayoutIr` entries keyed by semantic content hash, tool sidecar hash, display renderer version, runtime renderer generation, stable renderer cache key, and render-context hash.
- Bundled transcript defaults install a stable renderer cache key. User renderers and renderer middleware opt into persisted DisplayIR with `opts.cache_key`; omitting a cache key keeps runtime caching but disables persisted DisplayIR and row-index export for that renderer chain.
- `TranscriptProjection` hydrates display layouts before first projection and exports only history-valid entries for the current renderer generation/cache key once the renderer is known. Row-index entries carry the same renderer identity, so renderer invalidation rejects persisted heights and materialized rows without hashing Lua closures.
- Projection plans carry renderer generation/cache key, and `project_planned` rechecks the current Lua renderer identity before materializing a saved plan. If the renderer changed after planning, the row index and visible range are rebuilt under the current renderer before rendering.
- Width/theme changes continue to discard only row/materialized state; display layouts remain width/theme-independent. Renderer generation/cache-key changes clear display layouts, row indexes, and rendered rows.
- Coverage includes DisplayIR cache round-tripping, cold hydration without a row index avoiding recompilation, renderer-generation mismatch rejection, renderer-cache-key mismatch rejection, custom renderers without cache keys opting out of persistence, and planned projection renderer-identity rechecks.

Validation after implementation:

- `cargo build`
- `cargo test -p smelt-tui transcript_buf`
- `cargo nextest run --workspace`
- `cargo xtask gen-lua-docs`
- `cargo fmt && cargo clippy --workspace --all-targets -- -D warnings`
- `git diff --check`

### Phase 7 — Remove obsolete Rust renderer modules and old APIs

Goal: consolidate around the new architecture.

Work:

- Delete any remaining tool-body-only APIs or policy-only Rust renderer modules.
- Keep Rust modules only for primitive mechanics: markdown/code/diff/text wrapping/panel/gutter/etc.
- Update docs and Lua stubs.
- Regenerate Lua API docs.

Exit criteria:

- one display path;
- one layout IR;
- no buffer-leaf transcript API;
- no compatibility shims;
- code reads as if Lua-defined transcript layout was always the design.

Status after implementation:

- Audited transcript/content/runtime docs and code for `DisplayBlock::...`, `LuaLeaf::Buf`, `layout.leaf(buf)`, `smelt.layout.tool_output`, `set_tool_renderer`, tool-body-only APIs, and compatibility/shim markers; no transcript display leftovers remain.
- Removed the single-variant `DisplayBlock` wrapper. `DisplayModel` and persisted display-layout cache entries now store `LayoutIr` directly, with `session.ir.bin` payload format bumped to `FORMAT_VERSION = 3` for the serialized shape change.
- Renamed the remaining transcript display cache, module, counter, and test terminology from display blocks to display layouts (`content::display_layout`, `DisplayLayoutCacheEntry`, `display_layouts`).
- Removed the obsolete buffer-leaf carrier from content layout (`LuaLeaf::Buf` and generic `Leaf<B>`). Lua-returned layout leaves are now declarative primitives/source directives only, and traversal tests use generic leaf payloads instead of buffer IDs.
- The remaining TUI content renderer modules are primitive mechanics: generic `layout_ir`, markdown internals, source-view/diff/file rendering, wrapping, panel/gutter/cap/hbox composition.

Validation after implementation:

- `cargo build`
- `cargo xtask gen-lua-docs`
- `cargo nextest run --workspace`
- `cargo fmt && cargo clippy --workspace --all-targets -- -D warnings`
- `git diff --check`

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
- Use these measurements to prioritize follow-up architecture work. In particular, do not implement generic `layout.style` or dynamic `layout.elapsed` until the performance results and API cleanup value justify their scope.

Current validation notes:

- Release benchmark harness is now `cargo xtask bench-transcript-layout --runs N`, documented in `docs/transcript-layout-benchmarks.md`. It runs the ignored projection and navigation/search benchmark suites in release mode with one warmup sample and `--test-threads=1`, then prints per-run samples, mean±stddev tables, and structural counters for layout compilation, exact height measurement, and visible materialization.
- `cargo xtask bench-transcript-layout --runs 5` reported:
  - navigation/search on a warmed 8,000-block / 16,000-row transcript: `/needle-target` search + redraw `42.6±1.1ms`, Ctrl-D ×20 + redraws `4.0±0.0ms`, Ctrl-U ×20 + redraws `4.5±0.0ms`, `gg` + redraw `0.23±0.00ms`, `G` + redraw `0.23±0.00ms`.
  - `mixed_10mib`: `blocks=3404`, `rows=89587`, `first=188.0±0.9ms`, `resize=155.7±1.2ms`, `theme=156.2±1.0ms`, `scroll12=12.9±0.4ms`, `visible=0.6±0.0ms`, `fullcache=5.1±0.5ms`, `ironly=152.0±1.3ms`, `nocache=189.0±1.8ms`.
  - `markdown_4mib`: `blocks=540`, `rows=56699`, `first=74.2±1.2ms`, `resize=72.5±0.4ms`, `theme=73.0±0.6ms`, `scroll12=21.1±0.5ms`, `visible=1.1±0.0ms`, `fullcache=0.8±0.0ms`, `ironly=71.2±1.2ms`, `nocache=74.7±1.1ms`.
  - `tool_output_4mib`: `blocks=47`, `rows=1080`, `first=72.7±1.0ms`, `resize=70.3±0.5ms`, `theme=70.9±0.8ms`, `scroll12=712.6±2.8ms`, `visible=54.7±0.5ms`, `fullcache=41.2±1.5ms`, `ironly=69.3±0.1ms`, `nocache=72.5±0.4ms`.
  - `tiny_blocks_1mib`: `blocks=32112`, `rows=80279`, `first=235.5±6.0ms`, `resize=65.5±1.7ms`, `theme=67.1±1.0ms`, `scroll12=4.3±0.1ms`, `visible=0.3±0.0ms`, `fullcache=5.3±1.3ms`, `ironly=68.0±2.2ms`, `nocache=235.2±6.5ms`.
  - `huge_blocks_4mib`: `blocks=38`, `rows=45903`, `first=117.7±22.3ms`, `resize=141.3±45.3ms`, `theme=142.2±43.3ms`, `scroll12=429.0±203.5ms`, `visible=22.7±0.6ms`, `fullcache=13.8±0.3ms`, `ironly=92.4±0.7ms`, `nocache=102.0±15.0ms`.
- Counters confirm the algorithmic split: cold/no-cache compiles and measures every block; resize/theme compile 0 layouts but remeasure all exact heights; scroll/direct-visible/full-cache compile 0 layouts and remeasure 0 heights; IR-only hydration compiles 0 layouts but remeasures all exact heights.
- Hot-reload regression coverage now verifies a config-installed `smelt.transcript.extend_renderer` tool override changes after `reload_lua()`, rejects stale compiled IR/row indexes, and disappears when the config removes the extension.
- Current conclusion: persisted row-index + display-layout cache is the right resume mechanism. DisplayIR-only persistence is useful but not sufficient for fast resume because exact row heights dominate once Lua compilation is skipped. Theme invalidation is architecturally too broad because exact measurements are theme-independent. Width changes legitimately need a width-specific exact measurement index unless that width has already been measured. App-level `gg`, `G`, Ctrl-D, and Ctrl-U are cheap on warmed transcripts; search is materially more expensive than navigation but still below cold/resize measurement on the large projection workloads. Tool-output-heavy and few-huge-block workloads expose a separate visible-row materialization/rendering bottleneck: scrolling inside very large capped/raw-output blocks spends time rendering large child ranges even with 0 height remeasurement.

Recommended performance direction:

1. Split the current projection state into explicit derived artifacts with dependency-based invalidation: `DisplayLayout` cache keyed by semantic block + renderer identity, `ExactRowIndex`/measurement cache keyed by display-layout identity + width + thinking visibility, and visible rendered rows keyed by measurement + theme + viewport. This is a three-tier model because those are the real dependency boundaries; adding more tiers now would increase complexity without evidence, while two tiers would keep conflating exact measurement with rendered rows.
2. Preserve exactness by keeping measurement exact and cached. Do not estimate total rows. The architectural goal is to measure every block once per needed width/renderer identity, persist that exact index when safe, and never drop it on theme/scroll/viewport changes.
3. After the cache split, optimize exact measurement internals where counters still show all-block remeasurement: parsed/owned markdown sub-IR so markdown measurement does not reparse source, and width-keyed row-index retention for common widths.
4. Separately profile row-range materialization for large tool-output/huge-block workloads before adding a compositor abstraction. The release suite shows this is a different bottleneck from exact height measurement; fix it only with a direct row-range rendering/compositing design if profiling confirms the cost is in temp buffers/large child rendering.

### Phase 9 — Derived artifact cache architecture

Goal: make transcript projection state match the actual dependency graph instead of using one broad materialized-state invalidation path.

Target architecture:

- `DisplayLayout` / DisplayIR store: width-independent, theme-independent, viewport-independent `LayoutIr` compiled from semantic block data and renderer identity. Keyed by semantic block display key plus renderer generation/cache key. Lua renderer changes invalidate this tier; width, theme, and scroll do not.
- `MeasurementIndexStore`: exact row-index cache over `ExactRowIndex`, keyed by renderer identity, width, `show_thinking`, block order, and per-block display keys. It keeps an active exact row index and width-keyed remembered entries so revisiting a measured width hydrates exact rows instead of remeasuring. Persist only entries with a stable renderer cache key.
- `VisibleProjectionState`: rendered/materialized rows, visible block layout, backing-buffer projection marker, and full-text row cache. This tier depends on measurement, theme, viewport, and target buffer state. It is intentionally disposable.

Invalidation rules:

| Change | DisplayLayout store | Measurement indexes | Visible state |
| --- | --- | --- | --- |
| Scroll/viewport inside same measured width | keep | keep | rebuild or reuse only if previous materialized window covers the viewport |
| Theme change | keep | keep | drop rendered/materialized visible state; full plain-text row cache may remain if it has no theme data |
| Width change | keep | remember active exact index; hydrate target width if present, otherwise measure exactly | drop visible/full-row state for the old width |
| Renderer generation/cache-key change | drop | drop active and remembered indexes | drop all visible/full-row state |
| Semantic append with stable order prefix | retain unchanged block layouts | sync prefix and measure only appended/mutated affected suffix | existing projection key rejects stale visible rows |
| Semantic deletion/reorder/rewrite | retain matching live block layouts by block id/key | rebuild/sync exact index and remeasure invalidated nodes only | existing projection key rejects stale visible rows |
| Manual Lua reload / renderer extension update | renderer generation changes, so stale IR is rejected | stale row indexes rejected | stale visible rows rejected |

Success criteria:

- Theme invalidation causes `exact_height_measured_blocks = 0` for an already measured width.
- Width sequence `W1 -> W2 -> W1` reuses the cached exact row index for `W1` without recompiling DisplayIR or remeasuring heights.
- Existing row-index prefix/rewrite/order tests keep passing; exactness remains required for scrollbar, search, copy/yank, and vim navigation.
- Renderer reload regression continues to prove stale DisplayIR and row indexes are rejected after config changes.
- Benchmark suite is rerun and this plan is updated with before/after numbers, especially theme time and exact measurement counters.

Implementation status:

- `TranscriptProjection` now has explicit stores: `display_layouts`, `MeasurementIndexStore`, and `VisibleProjectionState`.
- Theme invalidation targets visible rendered state instead of discarding exact measurements.
- Width changes remember the active exact index before switching widths so warm width revisits can hydrate from memory.
- Regression coverage includes `theme_invalidation_preserves_exact_measurements` and `width_revisit_reuses_cached_exact_measurements`.
- Validation completed with `cargo check -p smelt-tui`, `cargo test -p smelt-tui transcript_buf`, `cargo test -p smelt-tui reload_recompiles_transcript_renderer_extensions_and_rejects_stale_ir`, `git diff --check`, and `cargo xtask bench-transcript-layout --runs 5`.

Post-refactor benchmark notes:

- Rerun `cargo xtask bench-transcript-layout --runs 5` under the normalized local performance conditions reports theme exact-height measurements as `0` for all workloads. Theme time changed from broad remeasurement to visible/materialized rendering cost: `mixed_10mib` `6.5±1.5ms`, `markdown_4mib` `1.7±0.1ms`, `tool_output_4mib` `70.8±11.1ms`, `tiny_blocks_1mib` `0.7±0.2ms`, `huge_blocks_4mib` `38.4±3.9ms`.
- Current workload means: navigation/search on warmed 16,000 rows: search `78.52±15.90ms`, Ctrl-D ×20 `8.05±0.98ms`, Ctrl-U ×20 `9.48±2.00ms`, `gg` `0.49±0.16ms`, `G` `0.44±0.08ms`.
- Projection workloads: `mixed_10mib` first `345.8±39.8ms`, resize `217.3±45.8ms`, theme `6.5±1.5ms`, scroll12 `19.6±6.2ms`, visible `1.0±0.3ms`, fullcache `7.3±1.3ms`, ironly `242.6±50.0ms`, nocache `310.2±22.8ms`; `markdown_4mib` first `117.9±17.6ms`, resize `121.1±17.1ms`, theme `1.7±0.1ms`, scroll12 `36.7±2.7ms`, visible `1.8±0.1ms`, fullcache `1.5±0.2ms`, ironly `116.7±11.0ms`, nocache `119.7±19.4ms`; `tool_output_4mib` first `114.2±6.7ms`, resize `111.9±20.3ms`, theme `70.8±11.1ms`, scroll12 `1302.8±273.2ms`, visible `87.0±11.6ms`, fullcache `67.9±7.9ms`, ironly `122.9±23.9ms`, nocache `125.8±24.1ms`; `tiny_blocks_1mib` first `434.0±94.8ms`, resize `110.8±10.2ms`, theme `0.7±0.2ms`, scroll12 `10.6±2.7ms`, visible `0.8±0.2ms`, fullcache `10.9±5.4ms`, ironly `121.0±11.2ms`, nocache `368.8±38.3ms`; `huge_blocks_4mib` first `161.7±23.5ms`, resize `174.7±18.4ms`, theme `38.4±3.9ms`, scroll12 `522.9±30.3ms`, visible `40.4±8.4ms`, fullcache `22.5±2.6ms`, ironly `157.4±15.6ms`, nocache `150.0±13.4ms`.
- Theme is now fixed as a measurement-invalidation issue. The remaining expensive theme/visible/scroll cases are row-range materialization/rendering in tool-output-heavy and few-huge-block workloads, which is the separate direct row-range renderer/compositor follow-up described above.

Large-resume implementation notes:

- Added `mixed_50mib` to the projection workload set and `--skip-nav` to the xtask runner. Use `cargo xtask bench-transcript-layout --runs N --workloads mixed_50mib --skip-nav` for projection-only large-workload iteration.
- Latest 50 MiB projection sample set: `blocks=17004`, `rows=447561`, `first=1036.5±196.3ms`, `resize=895.9±167.3ms`, `theme=5.4±1.0ms`, `scroll12=15.4±2.7ms`, `visible=2.2±0.4ms`, `fullcache=8.2±2.0ms`, `ironly=910.9±237.5ms`, `nocache=1103.5±228.5ms`, with `cache_row_indexes=1` and `cache_display_layouts=17004`.
- Counters on that sample isolate the dominant projection dependency: full-cache hydration loads the exact row index and measures `0` blocks, while DisplayIR-only hydration measures all `17004` blocks. Exact row-index persistence is therefore the difference between ~8ms projection resume and ~0.9s exact measurement at 50 MiB. No-cache cold projection adds LayoutIR compilation and lands near ~1.1s.
- `session.ir.bin` is now a disposable local cache with clean format version `1`: header + independently encoded row-index and DisplayIR payload sections. There is deliberately no compatibility path for older IR sidecars; stale/corrupt files miss and are recomputed.
- Canonical sessions now write inspectable `meta.json` + `history.jsonl` + `content.txt`. Loading prefers JSONL and keeps short-lived `COMPAT(session-json-monolith)` support for old monolithic `session.json` sessions; opening a legacy session immediately rewrites it to the split format and removes `session.json` after the new files exist.
- True-resume benchmark fixture (`transcript_true_resume_benchmark_suite`) now measures save/load/cache-read/rebuild/render through the real session path. 50 MiB release sample: `history_items=28206`, `rows=860272`, `build=79.7ms`, cold `first=985.6ms`, JSONL `load=57.2ms`, IR cache read `11.6ms`, rebuild `73.3ms`, hydrated render `3.5ms`.
- `layout.style(child, opts)` is implemented as an inherited LayoutIR style wrapper for `hl`/`hl_group`, `fg`, `bg`, `dim`, `bold`, and `italic`. Panel remains the full-width background/chrome primitive; style is for inherited text styling.
- `layout.elapsed(block.elapsed, opts?)` is implemented as a render-time LayoutIR leaf. Rust exposes `block.elapsed` on tool blocks and resolves the current tool elapsed from `ToolState` during render when available. The bundled default tool header now places this leaf in the header so elapsed ticks update dynamically.
- Elapsed-only tool-state ticks no longer affect `ToolState::display_hash()`, so pending elapsed updates can reuse cached DisplayIR instead of recompiling block layouts.
- Default tool-body policy now caps only raw output via `render_tool_output` (`keep="tail"`, marker above). Structured tool renderers such as diffs, file views, notebook previews, and plan summaries are guttered but not capped by the default wrapper; custom renderers opt into caps explicitly when their own body can explode.

Resume architecture sequence, preserving exact row/search/scroll semantics:

1. **Clean IR cache v1, no legacy.** Keep the current projection tiers; persist exact row indexes and DisplayIR as independently decodable cache sections, and treat all prior sidecars as disposable misses.
2. **True resume benchmark + diagnostics.** Measure the real session path separately from projection-only fixtures so JSONL parse, transcript rebuild, IR read, and hydrated render costs stay visible.
3. **Canonical session format.** Use `meta.json + history.jsonl` as the inspectable canonical storage. Keep only documented short-lived monolith compatibility.
4. **Layout style semantics.** Use `layout.style` for inherited foreground/background/text attributes and `layout.panel` for full-width background panels.
5. **Dynamic elapsed primitive and cache-stable ticks.** `layout.elapsed(block.elapsed, opts?)` is available as a render-time leaf; the bundled header uses it, and elapsed-only updates do not change DisplayIR cache keys.

Recommended next slice: run the focused validation/benchmark pass for the elapsed/header and raw-output policy changes, then only consider deeper storage or compositor rewrites if measured bottlenecks remain.

Exit criteria:

- `cargo fmt && cargo clippy --workspace --all-targets -- -D warnings` passes;
- `cargo nextest run --workspace` passes;
- large baseline improves by avoiding full-block render measurement;
- no current storybook/UI snapshots regress unintentionally.

## Resolved design choices

- Expose one Rust-facing root override point: `smelt.transcript.set_renderer(fn, opts?)`. A stable `opts.cache_key` opts custom renderers into persisted DisplayIR; omitting it deliberately disables persistence for that renderer chain.
- Provide Lua-level root-renderer composition with `smelt.transcript.get_renderer()` and `smelt.transcript.extend_renderer(name, fn)`. This is middleware around the single root renderer, not a Rust per-kind/per-tool registry.
- Provide `smelt.transcript.invalidate_renderer()` for renderer-affecting closed-over Lua state changes that do not go through `set_renderer`, `extend_renderer`, extension removal, or Lua reload.
- Transcript/default/preview renderers return declarative LayoutIR only. Imperative buffer/render/paint APIs remain low-level escape hatches for windows, overlays, prompt/status bars, pickers, and debug panels; they are not transcript display APIs.
- Do not add `smelt.transcript.renderer.set(kind, fn)`, `set_tool_renderer`, or a Rust-owned tool/body renderer registry.
- Bundle default renderers as normal Lua modules. The default root renderer calls `smelt.transcript.defaults.render(block, ctx)`; users can call, compose, copy, or ignore those helpers.
- Keep per-block and per-tool dispatch in Lua. Built-in tool body functions may live in a Lua table inside the defaults module, but that table is implementation/composition code, not a Rust API.
- Do not add `layout.tool_header(block)`. Tool headers are default Lua composition, exposed as `smelt.transcript.defaults.render_tool_header(block, ctx, opts?)` for reuse.
- Implement dynamic pending elapsed as a lower-level primitive: `layout.elapsed(block.elapsed, opts?)`. Lua decides placement/chrome; Rust resolves current tool state while rendering the leaf.
- Preserve current mode one-row behavior with `layout.line` unless deliberately changed with snapshots.
- Keep exact user-text highlighting via general span/text primitives plus Rust-provided semantic annotations or narrowly mechanical tokenization; do not hide user-message panel chrome in Rust.
- Implement tail/head capping as generic `layout.cap` IR with numeric row counts from `ctx.limits`; do not keep `smelt.layout.tool_output`.
- Do not special-case command strings containing `tail`; the row-selection semantics belong to the default raw-output composition over `layout.cap`.

## Remaining open design decisions

1. Should mode blocks always clip to one row exactly, or should `layout.line` offer configurable clip/ellipsis behavior?
2. What exact shape should Rust-provided user-text annotations take: precomputed spans on `block`, a general tokenizer primitive, or a helper in defaults that calls a mechanical tokenizer?
3. What are the precise `layout.style` inheritance and merge rules across text/runs/markdown/code, panels, gutters, caps, hboxes, and nested style wrappers?
4. Should `extend_renderer` support priority/load-order options beyond “later extensions run first,” or is named registration/removal enough for the first implementation?
