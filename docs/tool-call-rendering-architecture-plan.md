# Tool-call rendering architecture implementation plan

Status: Complete

## Purpose

Make tool-call transcript rendering one coherent public Lua customization surface.
Rust persists and exposes semantic facts, plans semantic groups, executes declarative
layout, and schedules refreshes. Lua owns every visual choice: status markers,
titles, summaries, elapsed durations, invocation times, group aggregation, styles,
alignment, bodies, and expanded children.

The default presentation is:

```text
* bash git status --short  1.2s                            14:37:03
  M crates/core/src/lua/runtime.rs
```

The complete title, including the leading `*`, is Lua-owned. Elapsed duration is
inline after the title, invocation time is at the far right, and the body is below
the header. Collapsed groups choose one invocation time in Lua. Expanded children
re-enter the same root renderer and middleware pipeline as standalone calls.

## Architectural decision

Use one canonical renderer for every semantic transcript node:

```lua
renderer(node, ctx) -> layout
```

A node may be an ordinary transcript block, an individual tool call, or a semantic
tool group. Rust must not select a separate presentation path for groups. Recursive
rendering uses `ctx.render(child, child_ctx)`, which re-enters the configured root
renderer so replacements and middleware apply at every depth.

Tool snapshots expose raw timing facts:

```lua
tool.called_at_ms
tool.elapsed_ms
tool.elapsed_active
ctx.now_ms
```

Lua formats and places those values. Live presentation uses a generic declarative
refresh wrapper:

```lua
layout.refresh(view, { after_ms = 250 })
```

The wrapper adds no visible content. Rust records the earliest requested deadline
for the containing top-level render node, invalidates that node when due, and reruns
its Lua renderer. If the next result has no refresh wrapper, refreshing stops.

## Invariants

1. Invocation wall-clock time is durable semantic state and survives persistence,
   hydration, eviction, resume, and transcript reconstruction.
2. Rust does not format tool titles, elapsed durations, invocation timestamps, or
   group timing labels.
3. Every block, tool, group, and expanded child passes through one composed root
   renderer and the same middleware chain.
4. Groups contain semantic child snapshots. They do not name or invoke a separate
   Rust-side group renderer.
5. The complete visible tool header and body can be replaced from Lua.
6. Built-ins and plugins use the same public presentation API. Private
   `__tool_*` presentation registries do not exist.
7. Refresh requests are declarative, bounded, and scoped to a top-level cached
   render node. Nested requests compose by selecting the earliest deadline.
8. Hiding dynamic timing produces no refresh work.
9. Declarative layout remains serializable data. Lua callbacks are never stored in
   layout IR.
10. Existing transcript selection, wrapping, row-prefix, sparse hydration, cache
    bounds, and semantic group planning remain intact.
11. A tool invocation start is one semantic payload, including `called_at_ms`. Engine
    classification stores the tool call, parsed arguments, monotonic start, and wall
    timestamp once; every execution path refers to that record by stable index.
12. Tool-presentation registrations are immutable snapshots. Changing callbacks or
    cache keys requires re-registration so renderer generation and persisted cache
    identity cannot drift.
13. Focused presentation callbacks either return their documented semantic type or
    fail with a tool-and-field-specific plugin error.

## Public Lua model

### Semantic nodes

The root renderer receives a consistent node table. Ordinary fields remain
available for their relevant kinds. Tool nodes expose durable status, output,
summary, timing, and view-state facts. Group nodes expose group identity, aggregate
state needed for interaction, and ordered child nodes.

Expanded child rendering must use the context callback:

```lua
local child_layout = ctx.render(child, {
  view_state = child.view_state,
})
```

The callback preserves inherited context, applies child overrides, and invokes the
same composed root renderer. It must guard against invalid recursion without
creating a second renderer stack.

### Tool presentation

Expose one public tool-presentation registry used by both built-ins and plugins.
A tool presentation may define the complete renderer or focused semantic pieces
used by the default renderer. Registration snapshots the supported fields and the
getter returns a copy, so behavior and cache identity change only through explicit
re-registration:

```lua
smelt.transcript.register_tool("bash", {
  render = function(tool, ctx, presentation) ... end,
  title = function(tool, ctx) ... end,
  body = function(tool, ctx) ... end,
  draft = function(draft, ctx) ... end,
  compact = function(tool, ctx) ... end,
})
```

The exact final names should match existing Lua conventions and stay minimal. A
complete `render` override takes precedence. The default renderer composes `title`
and `body`; group summaries may use `compact`; draft UI may use `draft`. Execution
hooks such as summary generation and confirmation remain separate from transcript
presentation.

### Timing

- `called_at_ms` is Unix epoch milliseconds captured when invocation begins.
- `elapsed_ms` is the best known execution duration at snapshot time.
- `elapsed_active` says whether elapsed time can continue advancing.
- `ctx.now_ms` is one render-pass wall-clock value shared by parent and recursive
  children.

Default Lua helpers format duration and adaptive local invocation time. Invocation
time uses:

```text
same day       14:37:03
same year      mar 20 18:42:03
different year 2025 mar 20 18:42:03
```

The default active duration wraps its visible layout in `layout.refresh`. Date
formatting requests a refresh at the next local day or year boundary only when the
chosen label could change.

### Declarative refresh

`layout.refresh(view, opts)` accepts one child and exactly one positive scheduling
policy, initially `after_ms`. Lua converts it to a declarative layout node. Layout
compilation renders the child normally and accumulates the earliest monotonic
refresh deadline for the top-level compile result.

Relative deadlines are converted once per compile. Recursive nodes share the same
compile pass and deadline accumulator. Refresh metadata is not part of visible
measurement, text selection, or copied transcript content.

## Removed architecture

Delete the split and specialized mechanisms after replacement coverage exists:

- Separate Rust block and group renderer dispatch.
- Group callback registration used for presentation.
- Expanded children calling default rendering directly.
- `ElapsedSpec`, `LayoutLeaf::Elapsed`, and `LuaLeaf::Elapsed`.
- `smelt.layout.elapsed`.
- Rust `tool_elapsed_text` and elapsed-width measurement logic.
- Preformatted `elapsed_text` in Lua snapshots.
- Timestamp-specific private helpers used as cross-module registries.
- `__tool_body_renderers`.
- `__tool_draft_preview_renderers`.
- `__tool_collapsed_details`.
- `__tool_header_rest_prefixes`.

No compatibility wrappers or display settings are retained. This is a direct API
cutover.

## Implementation stages

### Stage 0: characterize user-visible behavior

- Preserve an end-to-end story for a standalone pending and completed tool.
- Preserve collapsed and expanded group stories.
- Add renderer tests proving the full asterisk, title, timing, and body can be
  replaced or omitted.
- Add middleware tests covering standalone tools, collapsed groups, and expanded
  children.

Exit gate: tests describe the desired public behavior before the old paths are
removed.

### Stage 1: finalize semantic timing

- Keep `called_at_ms` in persisted `ToolState`.
- Capture it at every real invocation start, including streamed drafts becoming
  calls.
- Replace preformatted elapsed snapshot fields with `elapsed_ms` and
  `elapsed_active`.
- Add one `ctx.now_ms` value for an entire root render pass and recursive children.
- Verify persistence, hydration, eviction, and resume.

Exit gate: Lua receives only the raw facts required to reproduce all timing UI.

### Stage 2: unify semantic node rendering

- Represent groups as semantic transcript nodes with ordered child snapshots.
- Route blocks and groups through the same Rust renderer invocation and cache
  compilation path.
- Replace group callback dispatch with ordinary root-renderer handling.
- Add `ctx.render` recursion through the composed renderer and inherited context.
- Preserve Rust group planning, hit testing, view state, stable identities, and
  cache invalidation semantics.

Exit gate: there is one renderer entry point and no presentation-only group
registry in Rust.

### Stage 3: public tool presentation

- Introduce the public tool-presentation registration API.
- Migrate all built-in tools and `plan_mode` to it.
- Make the default root renderer resolve and compose complete renderers, titles,
  bodies, drafts, and compact group details through that API.
- Keep execution, permission, preview-generation, and summary-generation hooks in
  their existing semantic subsystems.
- Delete private presentation registries and indirect bootstrap writes.

Exit gate: built-ins and third-party plugins customize tool transcript UI through
one documented API.

### Stage 4: generic refresh

- Add `layout.refresh` to Lua metadata and declarative layout IR.
- Compile its child identically to an unwrapped child while accumulating the
  earliest requested deadline.
- Store refresh metadata alongside each top-level cached layout.
- Integrate due-deadline invalidation into the render loop without polling every
  transcript block.
- Ensure replacement, eviction, hiding, and a rerender without refresh cancel old
  deadlines.
- Remove specialized elapsed layout, intrinsic-width handling, synchronization,
  and tests.

Exit gate: active elapsed display updates through generic invalidation, nested
requests select the earliest deadline, and no hidden timer creates work.

### Stage 5: default Lua presentation

- Implement one complete default tool renderer.
- Format the title, inline duration, right-aligned invocation time, and body in
  Lua.
- Implement collapsed group timing as ordinary Lua aggregation over children,
  choosing the latest invocation by default.
- Render expanded children through `ctx.render`.
- Keep timestamp and elapsed styles consistent with the dim tool-name treatment.
- Handle narrow widths without clipping status meaning or corrupting body layout.

Exit gate: storybook snapshots match the approved hierarchy at narrow and normal
widths.

### Stage 6: documentation and cleanup

- Update Lua type metadata for nodes, tool timing, render context, presentation,
  and refresh.
- Update plugin guide examples to show replacement and wrapping.
- Regenerate API reference documentation with `cargo xtask gen-lua-docs`.
- Remove stale APIs, duplicate formatters, fallback branches, old tests, and
  comments describing superseded behavior.
- Run the `simplify` review and incorporate its findings.

Exit gate: generated docs and implementation expose one coherent model with no
private presentation path.

## Required tests

### Semantic and persistence

- Invocation time is captured once in the classified-call record and reused by
  start events, permission paths, execution, and committed history.
- Repeated call ids retain distinct outcomes and timestamps through stable indexes.
- `called_at_ms` round-trips through canonical persistence.
- Hydration, eviction, rehydration, and resumed sessions preserve it.
- Active and completed snapshots expose correct raw timing facts.

### Renderer composition

- A root renderer can replace the complete title, including `*`, and the body.
- A renderer can omit duration and invocation time entirely.
- A renderer can reformat and reposition both timing values.
- Middleware wraps standalone tools, collapsed groups, and expanded children.
- Expanded children inherit context and can override child view state.
- Renderer invalidation updates all node kinds coherently.
- Mutating a registered presentation table or a getter result cannot change rendering
  or cache identity; explicit re-registration can.
- Invalid `render`, `title`, `body`, `draft`, and `compact` results identify the tool
  and callback field, then enter the normal root-renderer fallback path.

### Refresh

- `layout.refresh` preserves its child's visible measurement and selection data.
- A due request rerenders only the containing top-level node.
- Nested requests choose the earliest deadline.
- A new result replaces the previous deadline.
- Returning no refresh stops future rerenders.
- Evicted or replaced nodes leave no stale scheduled work.
- Static tool rendering schedules no periodic refresh.

### Default presentation

- Duration is inline immediately after the complete title.
- Invocation time is right-aligned.
- Body starts below the header.
- Same-day, same-year, and cross-year labels are correct.
- Boundary refresh updates adaptive labels after local midnight and New Year.
- Collapsed groups use the latest child invocation time.
- Expanded children show their own times and use the root renderer.
- Pending, successful, failed, denied, interrupted, draft, permission, narrow, and
  wrapped cases remain legible and selectable.

## Validation

Run focused Rust, Lua, harness, and storybook tests throughout implementation.
Inspect changed storybook snapshots rather than accepting bulk regeneration.
Finish with:

```bash
cargo fmt -- --check
cargo clippy --workspace --all-targets --features smelt-tui/harness -- -D warnings
cargo nextest run --workspace --features smelt-tui/harness
cargo xtask gen-lua-docs
git diff --check
```

The work is complete only when the unified architecture is implemented, stale
paths are deleted, the public API is documented, snapshots are pixel-checked, and
all validation passes.
