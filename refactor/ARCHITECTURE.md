# Architecture

Decisions and rationale for smelt's target shape. Pairs with two siblings:

- **`tui-ui-architecture.puml`** — class diagram. _How things relate
  structurally._ Types, fields, packages, arrows. No prose.
- **`REFACTOR.md`** — implementation plan. _How we get there._ Step-by-step
  migration, ordering, deletions per step.
- **This file** — _why things are shaped this way._ Decisions, the rules they
  produce, and the alternatives we rejected.

When the three drift, the diagram is canonical for structure, this doc is
canonical for intent, and REFACTOR.md is canonical for sequencing.

## North star — three commitments

Every change lands closer to all three, never farther from any.

1. **One rendering path.** The compositor (`ui::Ui`) owns every pixel. No
   parallel ANSI emitter, no cached "last painted" state outside the grid diff.
   Anything visible is a Window registered with the compositor.

2. **Lua surface = Rust API.** Lua plugins call the same Rust functions the
   core does. `smelt.*` is a thin wrapper around `Host` methods, not a
   translation layer. No stringly-typed action tokens. No effect log. Reads go
   live; writes go direct.

3. **Rust core, Lua features.** Rust owns pixel pushing (compositor, buffers,
   windows, layout, paint), generic capabilities (process/fs/http/parse/grep),
   and the engine boundary. Lua owns the _what_: which dialogs, tools, slash
   commands, and statusline segments exist; how panels compose; how the UI
   reacts. Same model as Neovim — small Rust core, everything user-facing is a
   plugin. **A feature living in Rust that could live in Lua is a bug.**
   Built-in Lua still needs a boundary: generic capabilities live in Rust,
   default product UX can live in built-in Lua, and optional workflows
   (background-process management, etc.) should be
   removable plugins over those primitives rather than baked into the core
   engine or default tool semantics.

## Crate map

Four crates, dependency arrows go one way, no cycles:

```
protocol ← engine ← core ← tui
              ↑
       runtime/lua/smelt/
```

(`tui` contains the `ui` module — Buffer, Window, Grid, LayoutTree,
Theme, VimMode — internally.  It is not a separate crate.)

| Crate                    | Role                                                                    |
| ------------------------ | ----------------------------------------------------------------------- |
| `protocol`               | Wire types, pure data, serde-serializable. Stable contract, no behavior. Includes `AgentMode`, `ReasoningEffort`, `PermissionOverrides`, `TurnMeta` |
| `engine`                 | LLM core. Provider abstraction, agent loop, MCP, cancel tokens, schema-aware streaming. Tools = schema + dispatcher trait only; impls live in Lua. **No permission policy, no multi-agent concept**. **Zero UI imports** |
| `core`                   | Headless-safe runtime. `Core` + `HeadlessApp`, subsystems, `Host` trait, `LuaRuntime`, `EngineClient`, Rust capabilities (`fs`/`http`/`permissions`/`process`/…), `Clipboard`/`KillRing`, and **`Buffer` + `BufferParser` (the content data type)**. No terminal imports |
| `tui`                    | Terminal frontend. `TuiApp`, event loop, terminal input editing, `UiHost` Lua bindings, rendering adapters (`to_buffer`, `prompt_buf`, `status`), `BufferParser` impls (markdown / diff / syntax / bash) under `tui::content::transcript_parsers/`, and the `ui` module (Window, Grid, LayoutTree, Theme, VimMode). Depends on `core` and `crossterm` |
| `runtime/lua/smelt/`     | The whole UX. Widgets, dialogs, commands, statusline, transcript/diff presentation, themes, tools |

**Litmus test for placement.** Stable wire contract → `protocol`.
Frontend-agnostic LLM/tool code → `engine`. Headless runtime +
capabilities + Lua + content data types → `core`. Terminal chrome +
rendering + TUI primitives → `tui`. How smelt looks/behaves → Lua.

**`Buffer` lives in `core`, parser impls in `tui`.** Buffer is pure
data — zero terminal deps. Headless reads through the same surface
as tui. Parser impls (markdown / diff / syntax / bash) stay in tui
because they pull from `syntect`, inline-md, diff-LCS — tui-only
crates. The `BufferParser` trait + the data type they write into
live in core.

## Surface model — two structures, two primitives

Everything visible on screen is a **Window** over a **Buffer**. Composition
happens in two structures only:

- **`Ui::splits: LayoutTree`** — base tiling. Mutable. Things in it claim
  space by definition.
- **`Ui::overlays: Vec<Overlay>`** — z-ordered overlays. Things in it never
  claim space.

Two primitives, one role each:

- **Buffer** — shared content: lines, extmarks, namespaces, undo, modifiable
  flag, and coordinate/source-display translation.
- **Window** — one viewport over one Buffer: cursor, scroll, selection,
  keymap recipe, focusability, gutters, and rendering. Window is the concrete
  interactive/renderable unit; no separate `Surface`, `Component`,
  `PanelWidget`, or `BufferView` layer exists in the target model.

Composition/chrome lives outside Window:

- **LayoutTree** — `Vbox { gap, separator, border, title, items } | Hbox
  { gap, separator, border, title, items } | Leaf(WinId)`. Composition shape.
  Container nodes carry four parent-node properties:
  - `gap: u16` — blank rows (Vbox) or columns (Hbox) between adjacent
    siblings. Default 0 = tight tiling.
  - `separator: SeparatorStyle` — line drawn on the middle row of the gap.
    Requires `gap >= 1`. `gap=0, separator=Solid` auto-inflates to `gap=1`.
  - `border: Option<Border>` — chrome around the container's whole rect.
    `Border = { style: BorderStyle, sides: Borders }` where `Borders` is a
    bitflag picking edges (`TOP | RIGHT | BOTTOM | LEFT`).
  - `title: Option<String>` — rendered on the `TOP` edge of the border.
    Only meaningful when `sides` includes `TOP`.

  Any container can have chrome. Convention is to use it on overlays only;
  the type system allows it anywhere. Want a 1-row blank line between
  transcript and prompt? `Vbox { gap: 1, separator: None, … }`. Want a
  divider? `Vbox { gap: 1, separator: Solid, … }`. Want a bordered
  composite? `Vbox { border: Some({ style: Rounded, sides: ALL }), title:
  Some("Find"), … }`.

- **Overlay** — placement + modality wrapper around a LayoutTree:
  `{ layout, anchor, z, modal }`. Carries no chrome itself — chrome lives on
  the LayoutTree's container nodes.

A dialog, picker, text input, notification, statusline, cmdline, or list is
not a separate Rust widget category in the target. It is one or more Windows,
composed in Lua into a LayoutTree/Overlay with keymap recipes and
buffer/extmark content. Rust may expose generic primitives (`buf`, `win`,
`overlay`, `layout`) and small helper bindings, but not a dialog-specific
framework or translator layer.

### The discriminator

> **"When this appears, does anything else shrink to make room?"**
>
> - **Yes** → splice into `Ui::splits`. Claims space.
> - **No** → push onto `Ui::overlays`.

No `claims_space` flag — the choice of structure _is_ the answer. This
collapses the old 6-variant `Placement` enum and removes the conceptual
ambiguity around "is this a docked float or a floating dock?"

### Concrete examples

| Surface              | Lives in   | Anchor / placement                            |
| -------------------- | ---------- | --------------------------------------------- |
| Transcript           | splits     | `Vbox` leaf, fills available height            |
| Prompt               | splits     | `Vbox` leaf, fixed height                      |
| Statusline           | splits     | `Vbox` leaf, 1 row                             |
| Stash indicator      | splits     | inserted between transcript and prompt         |
| Cmdline (open)       | splits     | inserted above statusline                      |
| Confirm dialog       | splits     | sub-Vbox inserted before dismiss               |
| Notification toast   | overlays   | `Anchor::Win { target: prompt, attach: Top }`  |
| Completer popup      | overlays   | `Anchor::Cursor(Below)`                        |
| Fuzzy finder         | overlays   | `Anchor::Screen(Center)` + chromed Vbox + modal |
| Hover tooltip        | overlays   | `Anchor::Win { target, attach: Right }`        |
| Draggable popup      | overlays   | `Anchor::Screen { row, col }` (mutate to drag) |

### Why "Overlay" not "Float"

The word "float" suggested a kind of Window. It isn't — it's a
placement+modality wrapper that points at Windows via a LayoutTree.
"Overlay" matches the discriminator language ("overlays splits, never
claims space") and removes the implication that there's a Window subtype.

## Buffer model

`Buffer` lives in `core` — pure data, no terminal deps. Tui owns
parser impls (markdown / diff / syntax) + rendering (`Window::render`,
theme resolve, grid diff). Headless reads through the same surface.

Vim-style: lines + extmarks in named namespaces + `modifiable` flag.

- **All decoration is extmarks** in named namespaces (highlights,
  virt-text, signs, conceals, source-byte mapping, click metadata,
  folds). One store, clear-by-namespace. Mirrors `nvim_buf_set_extmark`.
  P9.f: namespaces become integer handles minted by
  `smelt.api.create_namespace(name)`; the keyset takes nvim's option
  set verbatim plus smelt extensions (`yank`, `on_click`).
- **Theme references are highlight ids, not raw colors.** P9.e:
  extmark `Highlight` payload carries `HlGroup(u32)`; theme is the
  paint-time resolver.
- **`modifiable: bool`** is the data-layer edit guard, shared by all
  Windows over a Buffer. False for transcript / diff / notification /
  picker; true for prompt / cmdline / inputs.
- **Yank substitution per extmark.** `yank: Option<YankSubst>`
  (`Empty` elides, `Static(s)` substitutes, absent = literal source).
  Default — yank source bytes verbatim — is right for rendered
  markdown (`**bold**` copies as `**bold**`).
- **Per-buffer undo/marks** live on Buffer. Window owns no parallel
  edit buffer.
- **`Buffer::attach(spec)`** wires parsers. Spec carries parser kind,
  decoration namespaces, and optional `on_block` callback at semantic
  boundaries (block end, tool start/stop, turn end) — never per delta.
  No `on_lines` / `on_bytes` / `decoration_provider`.

`Buffer ≠ Grid`. Buffer persists across frames; Grid is the terminal
frame rebuilt each render. Compositor renders Buffers through Windows
into Grid.

Parsed metadata (diff LCS, syntect tokens) attaches as extmarks in
dedicated namespaces — computed once at ingest, persists with the
Buffer, invalidates on edit. No persisted IR cache file. Width-dep
layout runs at paint time.

## Rendering model — event-driven, diff-based, no dirty flag

The render loop is a single `tokio::select!` merging terminal events,
engine events, the Lua callback drain, and an animation tick. **Every
handled event triggers a render.** The render builds a fresh `current`
Grid by projecting all layers, diffs `current` vs `previous`, and flushes
only the changed cells.

Why this works:

- **Diff is the gate.** If nothing visible changed, the diff is empty and
  the flush writes zero bytes. The cost of "render every event" is just the
  projection pass + memcmp.
- **Idle CPU is zero.** When no events arrive, the loop parks. There is no
  fixed-FPS tick.
- **No `request_redraw` / no dirty flag.** Subsystems mutate state. The
  next time the loop wakes for any reason, the render reflects the change.
  No invalidation tracking, no per-subsystem signaling.
- **Resize / Ctrl-L / layer add-remove clear the diff baseline.** The
  compositor zeros its previous-grid before the next render; the diff
  becomes a full repaint by virtue of writing every cell. No special
  "force repaint" flag — the data is the signal.

### Things that change over time

Anything that should update without an external event — clock, spinner,
elapsed-time counter — drives a `Cell` (next section) from a timer. The
Cell setter wakes the loop; the next render resolves bindings against the
new value; the diff flushes the changed glyphs. No special path.

## Reactive cells — one primitive for state, events, and subscriptions

`Cells` is the single observer mechanism in the runtime. There is **no
separate autocmd registry** — autocmd subscription is `Cell::subscribe`,
autocmd fire is `Cell::set`, and named events without a value are
`Cell<()>`.

A `Cell<T>` is a typed, named slot that:

1. Holds a value.
2. On `set`, wakes the loop (one `select!` branch on the cells channel).
3. Notifies all subscribers, in registration order, queued and drained
   after the current `&mut` borrows release.

Lua surface: `smelt.cell(name):subscribe(fn)`,
`smelt.cell.new(name, init):set(value)`, and a glob form
`smelt.cell:glob_subscribe("*_changed", fn)`. `smelt.au.{on,fire}` is
a thin alias kept for nvim familiarity.

Built-in cells the runtime ships (stateful slots and pure events both):

| Cell                  | Type / Payload                       | Driven by                                  |
| --------------------- | ------------------------------------ | ------------------------------------------ |
| `now`                 | `DateTime`                           | `Timers` (1 Hz)                            |
| `spinner_frame`       | `u8`                                 | `Timers` (16 ms when something is animating) |
| `agent_mode`          | `AgentMode`                          | `SetAgentMode` applied                     |
| `vim_mode`            | `String` (`"Insert"` / `"Normal"` / …) | `TuiApp` publishes formatted `ui::VimMode`  |
| `model`               | `String`                             | `SetModel` applied                         |
| `reasoning`           | `ReasoningEffort`                    | `SetReasoningEffort` applied               |
| `confirms_pending`    | `bool`                               | `Confirms` registers / resolves            |
| `tokens_used`         | `TokenUsage`                         | `EngineEvent::TokenUsage`                  |
| `errors`              | `u32`                                | `TurnError` count                          |
| `cwd`                 | `String`                             | Process CWD                                |
| `session_title`       | `String`                             | `TitleGenerated`                           |
| `branch`              | `String`                             | Session branch swap                        |
| `history`             | `Cell<HistoryDelta>` (event)         | Message appended / mutated / cleared       |
| `turn_complete`       | `Cell<TurnMeta>` (event)             | Engine emits TurnComplete                  |
| `turn_error`          | `Cell<TurnError>` (event)            | Engine emits TurnError                     |
| `confirm_requested`   | `Cell<ConfirmRequested>` (event)     | Engine emits RequestPermission             |
| `confirm_resolved`    | `Cell<{handle_id, decision}>` (event)| User answered                              |
| `session_started`     | `Cell<SessionId>` (event)            | New session                                |
| `session_ended`       | `Cell<SessionId>` (event)            | Session cleared / forked                   |

Plugins create cells under their own namespace (`my_plugin:pending`,
`my_plugin:thing_happened`) to avoid collisions with built-ins.

### Spec escape hatch

Default segment is `{ bind = cell, fmt = … }`. For ad-hoc computation:
`{ call = fn, deps = { "cwd", "now" } }` — `call` runs only when a
dep cell changes, not per-frame. Push (cells) over pull (lualine
re-eval) keeps mlua off the hot path; idle CPU stays at zero.

## Input pipeline — three independent layers

```
TuiApp::vim_mode  (global VimMode)      what's the user trying to do?
       │
       ▼  routes key to focused Window's recipe
Window::keymap  (recipe id)             what keys are bound here?
       │
       ▼  recipe attempts mutation
Buffer::modifiable  (data guard)        final yes/no on edits
```

Each layer is independent and composes:

1. **Vim mode is global on App.** Pressing `i` enters Insert anywhere.
   Mode-specific keymaps live in the global keymap registry, indexed by
   `(mode, key)`. Two enums in the system:
   - `VimMode` — `Normal / Insert / Visual / VisualLine`. Defined in `ui`,
     owned by `TuiApp`. Not a wire type — the engine never sees it. `Core`
     stores `vim_mode` as a plain `String` cell; `TuiApp` publishes the
     formatted enum value.
   - `AgentMode` — `Normal / Plan / Apply / Yolo`. Permission-gating policy.
     Lives in protocol. Lua: `smelt.mode` (renamed from
     `smelt.agent.mode` to avoid collision with future `smelt.process`).
2. **Window keymap recipe** decides what keys do. Editor recipes bind
   `i/a/o/dd`; viewer recipes bind only `j/k/v/y`. Recipes are pure Lua,
   defined in `runtime/lua/smelt/widgets/`.
3. **`Buffer::modifiable`** is the final guard. Even a buggy editor recipe
   can't mutate a non-modifiable Buffer. Defense in depth.

There is **no `Vim` or `Completer` state-machine type in `ui`**. Per-buffer edit
history (registers, dot-repeat, undo) lives on Buffer. Per-Window cursor +
selection + Visual anchor stay on Window. Clipboard/kill-ring lives in
`Core` (it is a text-manipulation primitive, not a UI primitive). Completer decomposes into a ghost-text extmark in a "completer"
namespace + an Overlay for the picker dropdown + a keymap on the prompt
Window.

## Focus, hit-testing, and capture

Focus is semantic; hit/capture is geometric — separate enums.

- **FocusTarget** = `Window(WinId)`. Keyboard-addressable. Owns
  cursor / selection / mode label / keymap dispatch.
- **HitTarget** = `Window | Scrollbar { owner } | Chrome { owner }`.
  Mouse-addressable; can include non-focusable elements.
- **CaptureTarget** = `HitTarget` for in-flight gestures. Scrollbar
  can capture a drag without becoming focused.

Rules: `Ui::focus` is the SoT (read via `focus()`, write via
`set_focus(win)` which pushes prior onto `focus_history`).
`overlay_close` pops history back to the topmost still-existing
focusable Window. Click promotes focus only when hit resolves to a
focusable Window and no modal absorbs. Tab cycles are modal-aware.
Esc chain: focused Window first; if `Ignored`, `WinEvent::Dismiss`
fires on the enclosing Overlay. Cursor shape is global on `Ui`,
nvim-style.

## Frontends — Core, TuiApp, HeadlessApp

Split on the only axis that matters: does this code need a Ui?

- `Core` (in `core`) — `config / session / confirms / clipboard /
  timers / cells / lua / engine / files / processes / skills /
  frontend`. Event loop lives here. Zero terminal deps.
- `TuiApp { core, well_known, ui }` (in `tui`) — adds `well_known`
  WinIds + `ui::Ui`.
- `HeadlessApp { core, sink }` (in `core`) — adds JSON/text sink.

One binary, two entry points: `smelt` → `TuiApp`, `smelt -p "..."` →
`HeadlessApp`. No `EngineConfig.interactive` flag, no `if interactive`
branches. Borrow checker enforces the split.

### Side effects — `Host` and `UiHost`, no Effect enum

Two small traits, side effects are direct method calls. **`UiHost`
does not extend `Host`** — keeps `ui` free of tui-defined types.

`Host` exposes the 11 Ui-agnostic accessors (`config`, `clipboard`,
`cells`, `timers`, `engine`, `session`, `files`, `processes`,
`skills`, `frontend`, `confirms`). `UiHost` exposes `ui`, `focus`,
`buf_*`, `win_*`, `overlay_open`. `Core` impls `Host`; `TuiApp`
impls both (delegating Host); `HeadlessApp` impls only `Host`.

Lua bindings divide by trait. Host-tier (works in headless + tui)
lives in `core/src/lua/api/`; UiHost-tier (errors in headless) lives
in `tui/src/lua/api/`. Two cross-runtime cases keep typed forms
because they cross channels: engine boundary (`UiCommand`) and Lua
coroutine resumption (`host.lua().resume_task`).

No reducer, no Effect log. Observability via `tracing::trace!`.

## Lua surface contract

**Spec for hot paths, callback for events, coroutine for flows.** Strict by
design — far more so than nvim.

- **Hot paths (per-frame / per-token):** _spec only._ Statusline segments,
  gutter functions, decoration parsers, keymap RHS, theme/highlight, command
  metadata. Spec segments bind to `Cell`s; segments re-resolve only when a
  bound cell changes. The `{ call = fn, deps = {...} }` escape hatch runs
  Lua only on dep changes — never per-frame. Lua contributes data
  (extmarks, cell values); Rust paints.
- **Event handlers:** _callback._ Engine events, buffer attach block events,
  focus, click — fired by Rust at the right moment.
- **User flows:** _coroutine._ Picker, dialog, plugin tools — yield-driven,
  multi-step. One suspend mechanism (`mlua::Thread`); plugin code looks
  synchronous.

What we don't ship from nvim's surface: `decoration_provider`, `foldexpr`,
`on_lines`, `on_bytes`, `on_changedtick`. Hot-path callbacks aren't escape
hatches — they aren't shipped at all.

### Lua → Rust writes (TLS pointer)

Lua FFI functions reborrow `&mut App` from a TLS pointer
(`crate::lua::with_app(|app| ...)`) and call Host methods through it. Same
surface Rust handlers see — no Effect indirection. One pattern, used
everywhere.

`install_app_ptr(self)` is set at every `&mut App` site that drives Lua: the
main loop tick, startup `load_plugins`, the deferred callback drain. Lua
callbacks registered via `ui::Callbacks` would collide with the `&mut Ui`
borrow held during dispatch, so they're queued and drained after the borrow
releases.

### Bindings layout

Host-tier bindings live under `crates/core/src/lua/api/<name>.rs`:
`au.rs`, `cell.rs`, `clipboard.rs`, `cmd.rs`, `frontend.rs`, `fuzzy.rs`,
`grep.rs`, `mcp.rs`, `mode.rs`, `os.rs`, `parse.rs`, `path.rs`,
`permissions.rs`, `provider.rs`, `reasoning.rs`, `shell.rs`, `skills.rs`,
`spawn.rs`, `task.rs`, `timer.rs`, `tools.rs`.

UiHost-tier bindings live under `crates/tui/src/lua/api/<name>.rs`:
`buf.rs`, `win.rs`, `ui.rs`, `prompt.rs`, `statusline.rs`, `confirm.rs`,
`notebook.rs`, `diff.rs`, `syntax.rs`, `theme.rs`, `bash.rs`.

A few bindings remain in `tui/src/lua/api/` pending reclassification
(`engine.rs`, `fs.rs`, `process.rs`, `session.rs`, `html.rs`, `http.rs`,
`image.rs`, `model.rs`, `settings.rs`, `metrics.rs`, `transcript.rs`,
`vim.rs`, `keymap.rs`, `history.rs`).

Each binding declares whether it needs `Host` or `UiHost`; calling a
UiHost binding from a `HeadlessApp` raises a runtime error in Lua.

## Engine boundary — channels only

`EngineHandle` is the entire surface engine exposes to a frontend:

- `cmd_tx: Sender<UiCommand>` — frontend → engine
- `event_rx: Receiver<EngineEvent>` — engine → frontend

No trait impls, no callbacks, no UI types crossing the boundary. **No
engine-side state leaks** — the `processes`, `permissions`, and
`runtime_approvals` fields that used to live on `EngineHandle` moved to
`core::*` in P5. The channel-only shape powers headless frontends
without cherry-picking engine internals.

The engine/event bridge drains `event_rx` and updates buffers, cells,
and session state. Lua/UI actions send `UiCommand`s back across the
same channel boundary.

**Single event loop.** One `select!` merges `terminal_rx`, `engine.event_rx`,
`lua_callback_rx`, `cells_rx`, and the animation tick. Each event dispatches
through Host directly — no Effect serialization.

### Streaming pipeline

Streaming chunks must reach the screen with the lowest possible latency,
and must not pay FFI cost per chunk. The path is:

```
EngineEvent::TextDelta { delta }
   │
   ▼
Engine/event bridge (Rust)
   │
   ▼
Buffer::append(span)              ' pure mutation, no Lua
   │
   ▼
loop wakes → render → diff        ' chunk visible
```

**Lua never runs per chunk.** The Buffer's attach-spec `on_block` callback
fires only at semantic boundaries: end of a markdown block, start/stop of
a tool call, end of a turn. That's the right granularity for parsing,
extmark population, and any plugin reactions.

**Mid-block turn end.** When a turn ends with an open block (e.g. the
LLM stopped mid-fence), the parser flushes the open block on
`TurnComplete` with `incomplete: true` in the payload. Lua's `on_block`
handler treats it the same as a complete block (highlights what's
there); the `incomplete` flag is informational so plugins can render a
truncation marker if they want.

If the bridge remains a distinct type, it should own the full
event-to-buffer/cell fan-out. If it stays a thin wrapper around
`EngineHandle`, delete the wrapper and keep translation as plain reducer
code. The target is not the current middle state where both coexist. The
Buffer is the source of truth.

## Dialogs — one question per dialog

There is no Rust dialog framework in the target. A dialog is just a Lua
composition of generic primitives: one or more buffers/windows plus an
overlay/layout composition, with focus routing and a result coroutine.
**It owns no multi-question state.** The primitive is "open a dialog with one
prompt; coroutine yields until submit/dismiss."

That means no Rust-side dialog schema, no panel descriptor types, and no
translator from a dialog DSL into UI primitives. If a Lua helper exists
(`smelt.ui.dialog.open`), it is sugar over generic `buf` / `win` /
`overlay` / `layout` operations, not a second framework.

Multi-question flows are a Lua pattern, not a framework feature:

```lua
local answers = {}
for _, q in ipairs(questions) do
  local r = smelt.ui.dialog.open(spec_for(q))
  if r.action == "dismiss" then return end
  answers[q.id] = r.value
end
```

Each prior dialog has already returned by the time the next opens, so cancel
semantics are obvious (ESC the current dialog → loop returns) and the call
stack _is_ the state machine. No tab strip widget, no per-tab focus, no
"complete-all-tabs-to-submit" gate.

If a future flow wants "all questions visible at once," it builds a custom
dialog with N input panels in a Vbox — same primitives, different layout.
Not a framework concern.

## Lifecycle gates

Three small invariants the runtime enforces.

1. **Engine pauses while a confirm is pending.** `EngineClient` checks
   `Confirms::is_clear()` before pulling the next request from the queue.
   Resumes when the dialog closes. One gate, not scattered checks.
2. **Cancellation is cooperative.** Each Lua coroutine task carries a
   `CancellationToken`. Cancel = the token flips, in-flight async Rust calls
   return `Err(Canceled)`, and the coroutine resumes with that error so
   normal Lua flow handles cleanup. No forced kill.
3. **Dialog stacking is the compositor's existing layer stack.** Open order
   = z order. Top dialog has focus; below ones stay rendered (dimmed if the
   theme says so) but cannot be focused while a modal is on top. No special
   Rust framework code.

## Tools — Lua-owned, Rust-composed (FFI for intricate logic)

All tools live in `runtime/lua/smelt/tools/*.lua`. Engine holds only
schema + dispatcher trait. **Engine asks the dispatcher → Lua runtime
finds the impl → runs as a coroutine → returns the result.**
Coroutines yield on async Rust calls (process spawn, HTTP, FS).

Principle: **everything in Lua; FFI for intricate logic**. Anything
that's slow, fragile, or carefully-tested in Rust is exposed as a
`core::<cap>` function (`permissions` bash AST + workspace store,
`fs` atomic edit-with-mtime-check, `notebook` JSON munging, `grep`,
`html`, `http` with cache). Never split a tool "half Rust, half Lua".

Plugin parity: built-in and plugin tools land in the same registry;
no Plugin-vs-Core split. All permission policy is UX-side: engine has
no `Permissions` struct; the Lua hook returns `"allow" |
"needs_confirm" | "deny"`; `RequestPermission` ↔ `PermissionDecision`
is engine's full permission surface.

### Tool registration table

A Lua tool registers one table; Rust calls callbacks generically. No
callback's existence depends on the name. Fields: `name`, `schema`,
`hooks(args, mode, ctx)`, `run(call_id, args, ctx)`, optional
`summary(args)`, `render(buf, args, output, width)`,
`paths_for_workspace(args)`, `elapsed_visible: bool`.

**Eternal rule:** no tool/command/dialog/mode name matching in Rust
over a Lua-registered identifier.

### Drawing context is full, not partial

`render(buf, ...)` receives a `Buffer` userdata with the full API
(`set_lines`, `set_extmark` keyset, `attach`, namespaces, virt-text).
No `RenderCtx`-style enum-of-allowed-methods. Shipped renderers
(`smelt.bash.render`, `smelt.diff.render`, `smelt.syntax.render`,
`smelt.notebook.render`) sit *on top of* the Buffer API as
conveniences, not in front of it. Applies symmetrically to dialogs,
statusline, transcript blocks.

## Rust capabilities — parse-then-present pattern

`core::process`, `core::fs`,
`core::http`, `core::html`, `core::notebook`, `core::grep`, `core::path`,
`core::fuzzy`, `core::permissions` — generic primitives, _not_
tool-specific. Any Lua plugin composes from them. A statusline source
reads git state via `core::fs`. A custom command shells out via
`core::process`. A theme finds files via `core::fs::glob`. A picker
ranks candidates via `core::fuzzy`. The `bash` tool checks command
shape via `core::permissions.parse_bash`. Tools are just
one consumer.

`core::process` (foreground process spawning, used by short-lived
shell commands) is the primitive; background-process registry and
long-lived child IPC are extensions of the same surface. `core::process`
carries its
own ergonomic shape (output capture, exit code, timeout) so it
stands as its own module.

Each module is independent (no umbrella folder) and is bound to Lua via a
sibling file in `crates/tui/src/lua/api/<name>.rs`.

For parse-then-present work (markdown, diff, syntax), the pattern is:

1. **Rust does the fast pure parse** — yields structured output.
2. **Lua walks the result** and writes Buffer extmarks via `ui::buf::*`.

Hybrid: Rust hot path, Lua presentation policy. Same shape for any future
binding.

## What goes where

| Concern                                                   | Owner                                       |
| --------------------------------------------------------- | ------------------------------------------- |
| Pixel pushing (grid, diff, SGR)                           | `tui::ui`                                   |
| Buffer (content data type) + `BufferParser` trait         | `core`                                      |
| BufferParser impls (markdown / diff / syntax / bash)      | `tui::content::transcript_parsers`          |
| Window / cursor / scroll / selection / viewport           | `tui::ui`                                   |
| LayoutTree / Overlay / Border / Anchor / Constraint       | `tui::ui`                                   |
| Compositor event routing (focus-driven keys, hit-tested mouse) | `tui::ui::Ui`                          |
| Theme groups, highlight resolution                        | `tui::ui::Theme`                            |
| Engine handle, agent loop, providers, MCP                 | `engine::`                                  |
| Wire types (UiCommand, EngineEvent, AgentMode, ReasoningEffort, …) | `protocol::`                          |
| Permission policy (rules, modes, runtime approvals, workspace store, bash AST) | `core::permissions` capability        |
| Generic capabilities (process / fs / http / permissions / …) | `core::` (one module each)            |
| App subsystems (Session, Confirms, Clipboard, Timers, Cells) | `core::Core`                              |
| Headless frontend (HeadlessApp)                           | `core::`                                     |
| Terminal frontend (TuiApp, event loop, content rendering) | `tui::`                                     |
| Lua bridge — Host tier (TLS pointer + Host bindings)      | `core::lua::api/<name>.rs`                   |
| Lua bridge — UiHost tier (buf / win / ui / statusline)    | `tui::lua::api/<name>.rs`                   |
| Engine ↔ Lua bridge (RequestPermission, queue_event)      | `core` engine-event bridge / reducer         |
| Tool dispatch (ToolDispatcher impl)                       | `core` Lua runtime                           |
| Slash commands (`/model`, `/theme`, `/resume`, …)         | Lua                                         |
| Dialogs (confirm, permissions, agents, rewind, …)         | Lua                                         |
| Tools (bash, read, write, edit, glob, grep, …)            | Lua (compose Rust capabilities)             |
| Statusline content                                        | Lua sources via `smelt.statusline.register` |
| Themes                                                    | Lua (`runtime/lua/smelt/colorschemes/*.lua`) |

**Test for "Rust core" vs "Lua feature":** would a _different_ plugin reuse
this exact code? Yes → Rust primitive. No → Lua feature.

## Why not ratatui

- Immediate-mode rebuilds every frame; we want grid diffing.
- No persistent windows with cursor / scroll / focus.
- No z-order beyond render order.
- `ratatui::Buffer` = cell grid; our `ui::Buffer` = content model.

We borrow only the cell-grid concept as the intermediate rendering surface.

## Why `ui` is a distinct module inside `tui`

- Forces clean boundaries — `tui::ui` cannot import `engine` or `core`.
  It only sees `protocol` types (via `tui` re-exports) and `crossterm`.
- Testable in isolation: buffer/window/layout/grid logic uses fake grids and
  writers. Terminal raw-mode and crossterm event adaptation stay in `tui`.
- The public API surface _is_ the API. `pub` items in `tui::ui` are the
  contract; `tui` code outside the module uses them, headless code never
  sees them.

## Why `core` is a separate crate

- Headless frontends (one-shot CLI, server, GUI) can depend on `core` without
  compiling `crossterm`, `syntect`, or grid diffing.
- `core` owns everything that is not terminal-specific: session, tool dispatch,
  capabilities, cells, timers, the Lua runtime. `tui` owns only the chrome.
- `core` depends on `protocol` and `engine` only.  `tui` depends on
  `core` and `crossterm`.

**Current state:** `crates/core` extracted (P8.e). `ui/` absorbed
into `tui::ui` (P8.a). `term/` dissolved (P8). `Buffer` + `Style` +
`UndoHistory` + `BufferParser` moved to `core` (P9.b);
`tui::ui::{buffer,undo,id}` are re-export shims. Host-tier Lua
bindings in `core/src/lua/api/` resolve via `try_with_host`;
UiHost-tier in `tui/src/lua/api/` via `try_with_app`. Theme registry
landed (P1.0): `set(name, style)`, `link(from, to)`, `get(name)`.

## Code rules — eternal

These describe the *target* codebase. Refactor-process rules
(red-tree-OK inside phases, friction handling, doc sync) live in
`README.md`.

- **Neovim-inspired first.** When in doubt, do what Neovim does. Deviate
  only if it improves the outcome, and say why.
- **No `#[allow(dead_code)]`.** Use it, remove it, or leave the warning as a
  tracking marker.
- **No tool/command/dialog/mode name matching in Rust.** A `match name {
  "bash" => … }` or `if name == "bash"` over a Lua-registered identifier
  is a bug. The registration table (tool / command / dialog / mode)
  carries the metadata; Rust calls through it generically. Counterexample
  to this rule is treated as a regression, not a special case.
- **Drawing context is full, not partial.** Lua callbacks that paint
  receive a `Buffer` userdata with the full Buffer API
  (`set_lines` / `add_highlight` / `set_extmark` / `attach` / namespaces
  / virtual text). No `RenderCtx`-style enum-of-allowed-methods. The
  shipped Rust renderers (`smelt.bash.render`, `smelt.diff.render`,
  `smelt.syntax.render`, `smelt.notebook.render`) sit on top of the
  Buffer API as conveniences, not in front of it.
- **No deferral on size.** A change that improves the codebase is worth
  doing, regardless of how big the refactor is. Implementation size is
  not a con. Only defer changes that don't improve the codebase.
- **No stringly-typed dispatch.** `Callback::Lua` / `Callback::Rust` are the
  behaviour mechanism. `KeyResult::Action(String)` survives inside `ui` only
  as the widget→container internal protocol.
- **No state mirrors in `LuaShared`.** App state stays on App and is read
  live via `with_app`. `LuaShared` holds only genuine Lua-runtime state
  (handle registries, atomic counters, the coroutine runtime, the deferred
  invocation queue).
- **No cost calculus on effort.** Refactor size, time-to-land, and
  migration difficulty are not trade-offs. Only outcome quality counts:
  correctness, modularity, simplicity, maintainability.
- **Comments answer "why", not "what".** Default to writing none. Don't
  reference the current task / fix / caller — that belongs in the commit
  message.
- **One commit = one architectural move** (post-refactor norm). A migration
  that deletes the old surface alongside the new is one commit. Adding the
  new while leaving the old in place is incomplete.
- **Green tree every commit** (post-refactor norm).
  `cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo nextest run --workspace`.
  *During the refactor this is relaxed inside a phase — see `README.md`.*

### Testing TUI changes

`cargo nextest run` covers unit-testable logic. For visual behaviour
(dialog rendering, layout, selection), drive the real binary in a tmux
side pane against a local OpenAI-compatible LLM endpoint.
**You're already inside tmux** — never `tmux kill-server` /
`kill-session` / `new-session`. Split the current window:

```bash
tmux split-window -h -t <session:window> -c <worktree-path> -P -F '#{pane_id}'
tmux send-keys -t %ID 'cargo run -- --api-base http://0.0.0.0:8080/v1 --model Qwen/Qwen3.6-27B' Enter
tmux capture-pane -t %ID -p | tail -N
```

Stop with `tmux send-keys -t %ID C-c`. Close the pane with
`tmux kill-pane -t %ID`.

Debug with `eprintln!` — stderr lands in
`~/.local/state/smelt/logs/stderr.log`. Tail it from another shell. Strip
the `eprintln!`s before committing.

### UI conventions

- Dialog titles are lowercase (`resume (workspace):`, `permissions`).
  Uppercase is reserved for proper nouns.
- Meta columns are `dim`, content is normal weight. Selection retints the
  whole row fg.
- Selection = fg-accent on the cursor row. No bg fill, no cursor glyph, no
  layout shift.

## Future multi-agent — optional plugin pattern, not engine concept

Engine has no agent concept. Any future multi-agent feature is an
optional Lua plugin over `core::process` long-lived IPC (`spawn`,
`send`, `on_event`, `wait`, `kill`). Child process can be anything —
agent, MCP server, long-running bash. Transcript renders these tool
calls the same way as any other tool call; fancier UI rides on a
custom Buffer attach + cells. Bidirectional async happens through
`on_event` updating Lua-side cells.

## Configuration — one format, one entry point

User config: `~/.config/smelt/init.lua`. Plugins:
`~/.config/smelt/plugins/*.lua`. Tools:
`~/.config/smelt/tools/*.lua` (P9.g auto-register). Project-local
(P9.g): `<cwd>/.smelt/{init.lua, plugins/*.lua, tools/*.lua,
commands/*.md}` — autoloaded after globals, gated by a first-load
trust prompt. Embedded autoloads under `runtime/lua/smelt/` are the
SoT for default UX; init.lua runs after and overrides.

No YAML/TOML, no settings registry. Every option is a Lua binding
argument: providers, permissions, MCP, theme, keymap, model defaults.

## Assumptions

- Single-threaded TUI loop; Lua runtime is `!Send`.
  `Arc<Mutex<…>>` boundaries for cross-thread state.
- TLS app pointer is sound: Rust never holds its `&mut dyn Host`
  borrow while Lua runs (synchronous, single-threaded FFI).

