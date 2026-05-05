# Refactor — sequencing the move to target architecture

**Sequencing plan only.** Meta-rules (greenfield / red-tree-OK /
friction handling / doc-sync / `P<n>.md` template / cold-start) live
in `README.md`. Target intent lives in `ARCHITECTURE.md` and the
puml. **The plan below is a sketch — better paths beat following it
to the letter; see `README.md`.**

## State of the gap (one paragraph)

`engine` is policy-free: core tools live in Lua, `Mode` gating is a Lua
tool-hook concern, and `engine/permissions/` has moved to `core::permissions`.
Engine emits `RequestPermission` and consumes `PermissionDecision` —
that's its full permission surface. `protocol` holds the stable wire
contract and shared types (`AgentMode`, `ReasoningEffort`, `PermissionOverrides`).
`VimMode` stays in `ui` — it is a UI-local text-editing state, not a
wire type.
`ui` has been rebuilt around `Buffer` (namespaces, extmarks, attach),
`Window`, `LayoutTree` (Vbox/Hbox/Leaf), `Overlay`, and `Theme`.
`BufferView`/`Component`/`PanelWidget`/`Surface` and the 6-variant
`Placement` enum are gone.
`core` is the headless-safe runtime layer: `Core`, `HeadlessApp`,
`Host`, subsystems, `LuaRuntime`, `EngineClient`, Rust capabilities
(`fs`, `http`, `permissions`, `process`, …), and `Clipboard`/`KillRing`.
It has no terminal imports and no `ui` dependency.
`tui` is the terminal frontend crate: `TuiApp`, event loop, terminal
input editing, `UiHost` Lua bindings, rendering adapters, and the `ui`
module (Buffer, Window, Grid, LayoutTree, Theme, VimMode). It depends
on `core` and `crossterm` only.
`core` is extracted into `crates/core` in P8 — not gated on a third
frontend.  `ui` absorbs into `tui` in the same phase.
`Host` (Ui-agnostic, in `core::host`) and `UiHost` (compositor-bearing,
in `tui::ui`) are the trait pair. `Cells` subsumes autocmds. `EngineClient`
owns the engine channel and gates on `Confirms`. The Lua UX layer
(`runtime/lua/smelt/`) owns widgets, dialogs, tools, statusline, and
colorschemes. All 15 core tools migrated from Rust to Lua, with
intricate logic exposed via `core::*` capabilities as FFI.

Everything else is downstream of those.

## Phase ordering at a glance

```
Pre-P0 baseline harness            ── L2 test harness + 5–10 scenarios
        │                              against today's binary; goldens
        │                              lock current behaviour
        ▼
P0  clear the deck                 ── delete what won't survive
        │
        ▼
P1  ui primitives                  ── Buffer, LayoutTree, Overlay, Window,
        │                              Theme, Ui facade, Host trait
        ▼
P2  tui app restructure            ── Core / TuiApp / HeadlessApp,
        │                              Host + UiHost, subsystems, Cells
        │                              (subsumes autocmds), Timers,
        │                              EngineClient, WellKnown, ToolRuntime (deferred)
        ▼
P3  rust capabilities + lua api    ── tui::parse/process/fs/http/...
        │                              /permissions, one file per smelt.*
        │                              namespace
        ▼
P4  lua takes the ux ✅            ── status.lua, dialogs/, modes.lua,
        │                              widgets/, colorschemes/ (P4.b deferred
        │                              to transcript-pipeline keystone)
        ▼
P5  tools to lua ✅                ── 15 Rust tool impls to Lua, mode
        │                              gating becomes a Lua hook concern;
        │                              intricate logic via FFI capabilities
        ▼
P6  streaming + lifecycle polish ✅ ── per-block callbacks, cooperative
        │                              cancel, confirms gate, cell-event
        │                              fan-out
        ▼
P7  finalize ✅                    ── docs, examples, dead-code sweep
        │                              (parity walk deferred to P10)
        ▼
P8  crate extraction ✅            ── extract `core` module into `crates/core`,
        │                              split Lua FFI by tier, eliminate `ui`
        │                              and `crossterm` deps from `core`
        ▼
P9  make architecture true 🚧      ── Buffer to core, transcript pipeline as
        │                              BufferParser impls, de-Rustify Lua
        │                              concerns (no name matching in Rust)
        ▼
P10 ship it                        ── saved-state cleanup, parity walk in tmux,
                                      final lint gate, doc-sync close-out
```

P1 is the load-bearing one. After it, P2/P3/P4 can interleave somewhat;
before it, nothing else has its target shape.

---

## Pre-P0 — Test baseline harness

Capture today's behaviour as goldens before demolition. L2 harness
(HeadlessApp + wiremock'd LLM + JSON snapshots) lands now so each phase
boundary can re-run scenarios and review diffs with `cargo insta review`.
See `TESTING.md` § L2 and `P0.md` for detail.

---

## P0 — Clear the deck

Delete orthogonal noise that needs no replacement (per-widget
selection-style shims, legacy mouse dispatch, buffer-list widget, etc.).
Structural deletions (BufferView, Component, Placement, PanelWidget) move
to P1.0 where each demolition is paired with its replacement in the same
commit. See `P0.md`.

---

## P1 — UI primitives (the load-bearing phase)

Rebuild `crates/ui` around `Buffer`, `Window`, `LayoutTree`, `Overlay`,
`Theme`, and the `Ui` facade. Everything downstream rides on this.
Sub-phases land independently; see `P1.md` for the full log.

- **P1.0** ✅ — Theme registry + paired structural deletions (BufferView,
  Component, Placement deferred to their replacement sub-phases).
- **P1.a** ✅ — `Buffer` rewrite: lines, namespaces, extmarks, `BufferParser`,
  soft-wrap keyed by `(changedtick, width)`. Tail (transcript pipeline
  onto `BufferParser`) deferred to P9.b.
- **P1.b** ✅ — `LayoutTree` (`Vbox`/`Hbox`/`Leaf`) with constraints + chrome.
- **P1.c** ✅ — `Overlay` replaces `Float`; dialogs, cmdline, picker,
  notifications all migrate to Overlay + Anchor.
- **P1.d** ✅ — `Window` becomes the only interactive unit; Component /
  BufferView / StatusBar / WindowView retire. Vim + completer decompose.
- **P1.f** ✅ — `Ui` facade: `dispatch_event`, focus, capture, render.

End of P1: `ui` compiles in isolation. `tui` consumes the new shapes in P2.

---

## P2 — TUI App restructure

Split `App` into headless-safe `Core` plus `TuiApp` / `HeadlessApp`
frontends. Carve subsystems (`Cells`, `Timers`, `Confirms`, `Clipboard`,
`Session`, `AppConfig`, `WellKnown`, `EngineClient`) out of the
106-field god-struct. Install `Host` + `UiHost` traits. Collapse the
event surface onto a single `select!` loop publishing through `Cells`.
See `P2.md` for the full sub-phase log.

- **P2.a** ✅ — Subsystem carve-outs + `Core` aggregate + frontend split.
- **P2.b** ✅ — `Host` / `UiHost` traits + `ui::Event` / `Status` / `WinEvent`
  supporting types + `Window::handle` collapse.
- **P2.c** ✅ — `Cells` reactive layer (subsumes autocmds).
- **P2.d** ✅ — `EngineClient` event bridge.
- **P2.e** ✅ — Single `select!` loop.

End of P2: tree is green. App is a thin coordinator over named
subsystems.

---

## P3 — Rust capabilities + Lua API split

Put generic Rust capabilities behind named modules (`fs`, `http`,
`grep`, `permissions`, `parse`, `process`, etc.) and split Lua
bindings to one file per namespace. See `P3.md`.

- **P3.a** ✅ — Capability modules land as `tui::<name>` or `core::<name>`.
  Engine utility-tool files fold into their respective capabilities.
- **P3.b** ✅ — Lua API reorganized to one file per namespace under
  `lua/api/<name>.rs`.
- **P3.c** ✅ — Missing namespaces bound: `smelt.cell`, `smelt.timer`,
  `smelt.au`, `smelt.clipboard`, `smelt.permissions`, `smelt.frontend`,
  `smelt.mode`, etc.

---

## P4 — Lua takes the UX

Move every "what does smelt look/behave like" decision out of `tui`
into `runtime/lua/smelt/`. Rust shrinks to capability provision and
pixel pushing. See `P4.md`.

- **P4.a** ✅ — `runtime/lua/smelt/` layout: widgets, dialogs,
  colorschemes, statusline, modes, cmd bootstrap.
- **P4.b** ⏸ — Transcript + diff parsers in Lua (deferred to P9.b
  transcript-pipeline keystone).
- **P4.c** ✅ — Reactive statusline via `smelt.statusline.register`.
- **P4.d** ✅ — Dialogs orchestrated in Lua over generic `buf` / `win` /
  `overlay` / `layout` primitives.
- **P4.e** ✅ — Slash commands fully Lua via `smelt.cmd.register`.
- **P4.f** ✅ — Modes registry in Lua.

---

## P5 — Tools to Lua

Migrate 15 Rust tool implementations from `engine/tools/` into
`runtime/lua/smelt/tools/`. Engine becomes schema + dispatcher only.
Mode gating becomes a Lua `hooks` concern. See `P5.md`.

- **P5.a** ✅ — Tool dispatcher trait shape (`ToolDispatcher` with `dispatch`
  + `evaluate_hooks`).
- **P5.b** ✅ — Core tools migrated to Lua; intricate logic stays in Rust
  as `core::*` capabilities.
- **P5.c** ✅ — Engine cleanup: `permissions/` → `core::permissions`,
  multi-agent concept deleted, `EngineHandle` channels-only.
- **P5.d** ✅ — Drop `config.yaml`; all config in `init.lua`.
- **P5.e** ✅ — Protocol rename pass (`Mode` → `AgentMode`, drop `Plugin`
  prefix, etc.).

---

## P6 — Streaming + lifecycle polish

**Goal:** lock down the engine→buffer streaming path and the
lifecycle gates (confirms gate, cooperative cancel, dialog
stacking).

- **Streaming pipeline.** `EngineEvent::TextDelta { delta }` →
  `EngineClient` → `Buffer::append` (Rust-only). Lua never runs per
  chunk. Buffer's `on_block` callback fires at markdown-block end /
  tool start/stop / turn end. Verify via `parse_marker` traces.
- **Confirms gate.** `EngineClient` checks `Confirms::is_clear()`
  before pulling the next request from `engine.event_rx`. Resumes
  when the dialog closes. One gate.
- **Cooperative cancel.** Each Lua coroutine task carries a
  `CancellationToken`. Cancel = the token flips, in-flight async
  Rust calls return `Err(Canceled)`, the coroutine resumes with
  that error so normal Lua `pcall` flow handles cleanup.
- **Dialog stacking** = `Ui::overlays` z-stack. Open order = z
  order. Modal-on-top blocks focus to lower overlays. No special
  framework code.
- **Esc chain** = focused Window's `handle_key` first; if `Ignored`,
  `WinEvent::Dismiss` fires on the enclosing Overlay; Lua handles.
- **Cursor shape global.** Single `Ui::cursor_shape`, not
  per-Window. Updated on focus change.
- **Cell-event fan-out.** Verify every state-changing event in the
  engine pipeline reaches the right cell setter (`agent_mode`,
  `model`, `reasoning`, `tokens_used`, `turn_complete`,
  `confirm_requested`, `history`, …). Each cell name has at least one
  built-in subscriber test asserting the callback runs. The
  `smelt.au.*` alias is exercised the same way.

---

## P7 — Finalize

- ✅ Sweep `#[allow(dead_code)]`, `// removed`, `// kept for now`
  comments. Anything that survived should not have those markers.
- ✅ Sweep `LuaShared` mirrors. App state is read live via `with_app`;
  the only thing in `LuaShared` is genuine Lua-runtime state
  (handle registries, atomic counters, coroutine runtime, deferred
  invocation queue).
- ✅ Update `README.md` and `docs/` for any user-visible changes
  (statusline reactive, dialog open API, tool registration shape).
  No user-facing changes from P6 required doc updates; existing
  keybindings reference already covers Esc-dismiss behaviour.
- ✅ Drop `tui-ui-architecture.puml` (the old diagram). Rename
  `tui-ui-architecture.puml` (was `tui-ui-architecture-target.puml`).
- ✅ Run the workspace through one full `cargo fmt && cargo clippy
  --workspace --all-targets -- -D warnings && cargo nextest run
  --workspace`. This is the *one* hard gate at the end of the
  refactor.
- ⏸ Walk a parity matrix by hand in a running TUI: triple-click yank
  on transcript / prompt / dialog buffer, drag-extend, esc chain,
  selection bg, vim modes, cmdline, picker, confirm dialog with
  diff preview, notification toast, statusline live update, theme
  switch. Visual behaviour is not test-covered — the human walk is
  the gate.

---

## P8 — Crate extraction

Extract `core` into `crates/core` and absorb `ui` into `tui`. Result:
4-crate architecture (`protocol ← engine ← core ← tui`). `core` has
zero `crossterm` / `ui` imports. See `P8.md`.

- **P8.a** ✅ — Purge terminal dependencies from `core`: move TUI-specific
  files to `tui/src/app/`, dissolve `term/` module.
- **P8.b** ✅ — Break `core → ui` dependency: Clipboard + KillRing move to
  `core`; `VimMode` stays `tui`-only.
- **P8.c** ✅ — Split Lua FFI by tier: Host-tier bindings → `core`,
  UiHost-tier bindings stay in `tui`.
- **P8.d** ✅ — `with_app` returns `&mut dyn Host` so `HeadlessApp` drives
  Lua without terminal types.
- **P8.e** ✅ — Physical crate split: create `crates/core/Cargo.toml`,
  move modules, absorb `crates/ui/` into `tui/src/ui/`.
- **P8.f** ✅ — Move LuaRuntime/LuaShared core pieces and Host-tier API
  modules into `smelt-core`. Tui's `LuaShared` wraps `Arc<core::LuaShared>`;
  `host_read!` macro and `try_with_host` TLS dispatch land in `core::host`.


---

## P9 — Make the architecture true

**Goal:** close the deferral chain (`P1.a-tail → P4.b → old P9.b`) and
purge every place where Rust still encodes a Lua-shaped decision. Three
fat sub-phases sequenced by dependency. Full detail in `P9.md`.

- **P9.a** ✅ — Well-known window IDs moved from `ui` to `app`;
  `win_open_split` collision-tolerant; `prompt_data.rs` renamed to
  `prompt_buf.rs`.
- **P9.g** ✅ — `buffer::SpanStyle` unified with `grid::Style`; fixes
  strikethrough rendering bug.
- **P9.b.0/b.1** ✅ — Dead `cache_dirty` field deleted; persisted layout
  cache deleted; `TranscriptSnapshot` moved from `core` to `tui`; Lua
  `render` hook + `RenderCtx` introduced for tool output.
- **P9.b** 🚧 — **`Buffer` to `core`.** The keystone. `Buffer` +
  `BufferParser` + extmark types + `BufId` / `LUA_BUF_ID_BASE` +
  `UndoHistory` live in `core`. `core::style::{Color, Style}` is a
  frontend-neutral mirror of crossterm's enum; tui converts at the
  SGR-emit boundary. Core has zero terminal deps. `Theme` stays in
  `tui`. Lua `render` hook + `render_tool_body` migrate to write `&mut
  Buffer`; `RenderCtx`, `DisplayBlock`, `DisplayLine`, `SpanCollector`,
  `layout_out.rs`, `transcript_present/` rendering glue all delete.
- **P9.c** ⏸ — **Transcript pipeline as `BufferParser` impls.** One
  parser per `Block` variant in `tui::content::transcript_parsers/`.
  `BlockHistory.artifacts` deletes. Width-independent parsing moves to
  ingest time as namespaced extmarks. Pulls in: prompt wrap
  unification, copy/yank unification, responsive-bar dedup.
- **P9.d** ⏸ — **Tool hook owns the decision.** The five
  `tool_name == "bash" | "web_fetch"` matches in `permissions/`,
  `agent.rs`, and `transcript_model.rs` are one architectural
  mistake. Fix: tool's `hooks(args, mode, ctx)` returns
  `{ decision, summary?, confirm_message?, approval_patterns?,
  paths_outside_workspace? }` and Rust honors it. `needs_confirm +
  approval_patterns + preflight` collapse into `hooks`. Permission
  rules in Rust become a passive store queried via Lua helpers.
  Deletes `decide_base`, `extract_tool_paths`, `is_auto_approved`
  bash branch, `ActiveTool::elapsed` match, `agent.rs` cmd_summary
  branches, `confirm.rs::is_bash`, `statusline` mode→glyph map,
  `dialogs/confirm.lua::fill_preview` (→ `tool.preview`). Eternal
  rule extends to Lua dispatch: **no tool/command/dialog/mode name
  matching anywhere shared.**
- **P9.e** ⏸ — **HlGroup-id model.** Buffer extmarks carry semantic
  `HlGroup(u32)` instead of raw `Style` / `Color`. Theme is the
  paint-time resolver. Theme switches stop rewriting buffers. `Color`
  / `Style` survive only at the paint boundary.
- **P9.f** ⏸ — **Unified extmark keyset (nvim parity).** Extend
  `core::buffer::ExtmarkOpts` with `priority`, gravity, `sign_text`,
  `hl_eol`, `hl_mode`, `conceal`, `id`, `virt_text_pos`, `virt_lines`.
  Lua surface collapses to `smelt.api.create_namespace(name)` +
  `smelt.buf.set_extmark(buf, ns, row, col, opts)`. `add_highlight`
  / `add_dim` retire. EmmyLua stubs deferred post-P10.
- **P9.g** ⏸ — **Plugin auto-discovery + project-local config.**
  Plugins are the only user-facing concept: `<config>/init.lua` +
  `<config>/plugins/*.lua` + `<cwd>/.smelt/{init.lua,plugins/*.lua}`
  (project content trust-gated, hash-based). A plugin file may
  register tools/commands/keymaps/anything — no separate `tools/`
  user folder. Existing `commands/*.md` markdown commands keep
  loading via the autoloaded `custom_commands.lua` plugin (extended
  to scan project-local once trust clears).
- **P9.h** ✅ — **Cell `(new, old)` payload.** Cells store `prev`;
  subscribers fire `fn(new, old)`, glob subscribers fire
  `fn(name, new, old)`. Other hook-surface ideas (typed names, more
  event cells, prompt-section deps, stale-task tagging) deferred
  until a real consumer needs them.
- **P9.i** ⏸ — **Provider middleware.** Neutral `ProviderRequest` /
  `ProviderResponse` between `EngineClient` and the kind-specific
  serializers; `EngineClient` carries `Vec<Box<dyn ProviderMiddleware>>`
  with return-payload `before_request(req) -> req` +
  `after_response(resp) -> resp`. Plugin-extensibility seam for
  redaction / prompt rewriting / A/B swaps. ~500 LOC.
- **P9.j** ✅ — **Loader override search path.** mlua `require`
  searches `<cwd>/.smelt/runtime/?.lua` →
  `<XDG_DATA_HOME>/smelt/runtime/?.lua` → `include_str!`'d embedded.
  Users override individual UX files without forking. New
  `engine::data_dir()` accessor.
- **P9.k** ⏸ — **Honor `ToolExecutionMode::Parallel`.** Already in
  protocol; agent loop runs all tools sequentially. Read-only tools
  (`read_file`, `glob`, `grep`, `web_fetch`, `web_search`) marked
  `Parallel` get `tokio::join_all`.
- **P9.l** ✅ — **Embedded Lua tree.** `EMBEDDED_MODULES` (140
  lines) + `BOOTSTRAP_CHUNKS` (~30) + `AUTOLOAD_MODULES` (~30)
  collapsed to one `include_dir!("runtime/lua/smelt")` walk.
  Bootstrap stays explicit (semantic ordering); module enumeration
  and autoload come from directory walks (`tools/`, `commands/`,
  `plugins/`, `dialogs/`). Adding a built-in `.lua` under one of
  those dirs now requires zero Rust edits.

---

## P10 — Ship it

Final hygiene pass after P9 lands. Small, mostly mechanical. Closes
the refactor. See `P10.md`.

- **Saved-state cleanup.** `core::state::ResolvedSettings` and the
  surrounding JSON loader survived P5.d's "all config in `init.lua`"
  claim. Either drop persisted settings entirely (cache-shaped state
  becomes a small typed `SessionCache`) or move persistence behind a
  Lua API. Decide during P10.
- **INVENTORY drift sweep.** Walk every row marked `done` and verify
  reality matches; fix every `refactor/check.sh` red `✗`.
- **Parity walk in tmux.** Drive the binary by hand against a local
  endpoint and walk the visual matrix from `ARCHITECTURE.md § Testing
  TUI changes`. Visual behaviour is not test-covered — the human walk
  is the gate.
- **Final lint gate.** One `cargo fmt && cargo clippy --workspace
  --all-targets -- -D warnings && cargo nextest run --workspace`. Green
  is the ship condition.
- **Doc-sync close-out.** Decide whether `refactor/` archives or
  `ARCHITECTURE.md` + the puml stay as living docs outside the folder.

---

## A note on deferral discipline

Through P9 we tighten the deferral rule. Earlier phases postponed work
because it was *big* (transcript pipeline, three times). The new rule:

> **Implementation size is not a reason to defer.** If a change improves
> the codebase, do it now. Defer only when the change *does not improve
> the codebase* — a hypothetical optimization with no real consumer, a
> rewrite that competes with another in-flight migration, etc.

The P9 replanning folds in everything earlier phases dropped under
"too big." `P9.md` records the deferral chain and how it closes.

## What we are deliberately not solving here

- **Theme content.** A pretty default theme is its own task once the
  registry exists (P1.0 ✅). It is not in this refactor's path.
- **Plugin marketplace / install / discovery.** Plugins are
  Lua under `runtime/lua/smelt/plugins/` plus the user's
  `~/.config/smelt/init.lua`. Anything fancier comes after.
- **A second backend.** The `core` / `tui` split enables a GUI or
  server frontend, but no second backend is implemented yet.
- **Per-buffer / per-window options registry.** Nice-to-have but
  out of scope; revisit only if a real consumer shows up.

## One sequencing note

Order phases by *what unblocks what*, not by *what's easiest*. P1 is
the load-bearing one; do it first even though it's the largest. After
P1, P2/P3/P4 can interleave somewhat. Anything else (friction,
diagram drift, atomic-move discipline) is in `README.md`.
