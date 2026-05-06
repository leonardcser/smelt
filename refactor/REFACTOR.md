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
P9  make architecture true ✅      ── Buffer to core, transcript pipeline as
        │                              BufferParser impls, de-Rustify Lua
        │                              concerns (no name matching in Rust)
        ▼
P10 ship it 🚧                     ── saved-state cleanup, parity walk in tmux,
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
purge every place where Rust still encodes a Lua-shaped decision.
Status table + open sub-phase sketches + decisions + deferrals in
`P9.md`.

- **P9.a** ✅ — Well-known window IDs moved from `ui` to `app`;
  `win_open_split` collision-tolerant; `prompt_data.rs` renamed to
  `prompt_buf.rs`.
- **P9.g** ✅ — `buffer::SpanStyle` unified with `grid::Style`; fixes
  strikethrough rendering bug.
- **P9.b.0/b.1** ✅ — Dead `cache_dirty` field deleted; persisted layout
  cache deleted; `TranscriptSnapshot` moved from `core` to `tui`; Lua
  `render` hook + `RenderCtx` introduced for tool output.
- **P9.b** ✅ — **`Buffer` to `core`.** The keystone. `Buffer` +
  `BufferParser` + extmark types + `BufId` + `UndoHistory` live in
  `core`. `core::style::{Color, Style}` mirrors crossterm; tui
  converts at SGR-emit. Lua `render` hook writes `&mut Buffer`;
  `RenderCtx` / `DisplayBlock` / `DisplayLine` / `SpanCollector`-as-IR
  all gone. Three pieces deferred to P9.c.
- **P9.c** 🚧 — **Transcript pipeline as `BufferParser` impls.**
  Per-`Block` parsers under `tui::content::transcript_parsers/`;
  prompt rendering through the same parser; one
  `copy_range(buf, range)` primitive replaces the divergent paths.
  Mandated by P10 entry conditions — open work, not deferred.
  See `P9.md` § P9.c.
- **P9.d** 🚧 closeout — **Tool name matches deleted from shared
  Rust.** Mostly landed (`decide_base` bash/web_fetch,
  `ActiveTool::elapsed`, `agent.rs` cmd_summary,
  `extract_tool_paths`, `confirm.rs::is_bash`, statusline glyph
  map). In-flight closeout collapses every remaining hardcoded
  `bash` / `web_fetch` / `mcp` field into a generic
  `subcommands: HashMap<String, _>` shape across `RawModePerms`,
  `RuleOverride`, `PermissionOverrides`, the Lua `set_rules` parser,
  and `is_auto_approved`. `smelt.permissions.check_bash` /
  `check_web_fetch` / `check_mcp` retire — replaced by generic
  `smelt.permissions.check(mode, tool_name, value)`.
- **P9.e** ✅ — **HlGroup-id model.** Buffer extmarks carry
  `HlGroup(u32)` ids; theme resolves at paint via
  `Theme::resolve(hl)`. Theme switches mutate `Theme.styles[id]`
  once instead of rewriting buffers. Anonymous interning via
  content hash keeps transitional sites working without naming.
- **P9.f** ✅ — **Unified extmark keyset (nvim parity).**
  `ExtmarkOpts` carries the full nvim option set (`priority`,
  gravity, `id`, `hl_eol`, `hl_mode`, `conceal`, `virt_text_pos`).
  Lua surface collapsed: `smelt.buf.create_namespace(name) -> u32`
  + `smelt.buf.set_extmark(buf, ns, row, col, opts)`;
  `add_highlight` / `add_dim` retired. `Rect` extracted to
  `ui/geometry.rs` (lone structural cycle gone).
- **P9.g** ✅ — **Plugin auto-discovery + project-local config.**
  Plugins are the only user-facing concept: `<config>/init.lua` +
  `<config>/plugins/*.lua` + `<cwd>/.smelt/{init.lua,plugins/*.lua}`
  (project content SHA-256-trusted via the new `core::trust` module
  + persisted to `<state>/trust.json`). New `/trust` slash command
  whitelists the current project's content. Untrusted project
  content silently no-ops with a startup toast; running `/trust`
  records the hash and the next launch loads it. Existing
  `commands/*.md` markdown commands keep loading via the autoloaded
  `custom_commands.lua` plugin.
- **P9.h** ✅ — **Cell `(new, old)` payload.** Cells store `prev`;
  subscribers fire `fn(new, old)`, glob subscribers fire
  `fn(name, new, old)`. Other hook-surface ideas (typed names, more
  event cells, prompt-section deps, stale-task tagging) deferred
  until a real consumer needs them.
- **P9.i** ⏸ — **Provider middleware.** Deferred — speculative
  seam without a consumer. Land when the first plugin needs
  redaction / A/B swap / cassette capture. Sketch in `P9.md`.
- **P9.j** ✅ — **Loader override search path.** mlua `require`
  searches `<cwd>/.smelt/runtime/?.lua` →
  `<XDG_DATA_HOME>/smelt/runtime/?.lua` → `include_str!`'d embedded.
  Users override individual UX files without forking. New
  `engine::data_dir()` accessor.
- **P9.k** ✅ — **Concurrent tool execution.** Protocol's
  `ToolExecutionMode::Concurrent` (default) is honored: the agent
  drives all dispatched tools through a `FuturesUnordered` so
  reads run concurrently. `Sequential` (e.g. `ask_user_question`)
  is deferred until peers finish, matching the original spec.
- **P9.l** ✅ — **Embedded Lua tree.** `EMBEDDED_MODULES` (140
  lines) + `BOOTSTRAP_CHUNKS` (~30) + `AUTOLOAD_MODULES` (~30)
  collapsed to one `include_dir!("runtime/lua/smelt")` walk.
  Bootstrap stays explicit (semantic ordering); module enumeration
  and autoload come from directory walks (`tools/`, `commands/`,
  `plugins/`, `dialogs/`). Adding a built-in `.lua` under one of
  those dirs now requires zero Rust edits.
- **P9.m** ✅ — **Single LuaRuntime instance.** `main.rs` stages
  every Lua load (autoload → user `init.lua` → global plugins →
  project plugins+init under trust gate) before construction, then
  hands the loaded runtime + `TrustState` to `TuiApp::new` by value.
  `TuiApp::start` only installs the TLS app pointer and emits the
  untrusted-project toast. `init.lua` runs exactly once.
- **P9.n** ⏸ — **Vim becomes Lua-extensible.** `ui/vim.rs` keymap
  dispatch (~30 free functions) is the only interactive surface
  whose bindings aren't Lua-registerable. Move per-buffer state
  (registers / dot-repeat / undo) onto `Buffer` (P1.d.5d), then
  flatten the dispatch table behind `smelt.keymap.set("normal", …)`
  / `smelt.keymap.set("visual", …)` so plugins can rebind motions
  and operators (P1.d.5f.2d). The state machine stays in Rust;
  recipes become Lua.
- **P9.o** ✅ partial — **UiHost TLS flip + Action enum cleanup.**
  Two pieces landed; the prompt-rewrite body did not.
  - **UiHost TLS flip (landed).** New `UI_HOST` slot in
    `crates/tui/src/lua/app_ref.rs` holds `*mut dyn UiHost`
    alongside the existing `APP` slot for the concrete `TuiApp`.
    `install_app_ptr` populates both; `with_ui_host` /
    `try_with_ui_host` reborrow through the trait object.
    Mirrors the Host-tier split P8.f landed (`CORE_PTR`).
    Decouples future frontends (StoryApp, alternative
    compositor) from the concrete TuiApp struct.
  - **Dead Action variants (landed).** `Action` enum returned
    by `PromptState::handle_event` shed three unreachable
    variants (`ToggleMode`, `CycleReasoning`, `Resize`) and the
    `Event::Resize` branch. The first two are intercepted by
    the global chord layer in `app/events.rs`; `Resize` was
    already handled above. 7 variants, down from 10.
  - **Full prompt-as-Window rewrite (declined).** Migrating
    `PromptState` (~1170 LOC) to a Window+Buffer+Lua-recipe
    shape moves complexity rather than reducing it: ~79
    `KeyAction` Lua bindings + a widget recipe stand in for
    the bespoke handler. Plugin extensibility is theoretical;
    no concrete consumer present (deferral rule). Same logic
    rejects the bundled `attachment.rs` and `prompt_wrap.rs`
    moves. Revisit when a concrete consumer materializes.
- **P9.x** 📝 — **Config binding fidelity.** Reject-unknown /
  fidelity bugs in config-time bindings (~100 LOC):
  `provider.register` accepts per-model fields (today drops
  temperature / top_p / pricing); `mcp.register` reads
  `type`/`timeout`/`enabled` and rejects unknowns; `smelt.settings`
  collapses to field access via metatable
  (`smelt.settings.vim = true` reads/writes/iterates; unknown keys
  error at the access site).
- **P9.r** ✅ — **Tool render returns `BlockLayout`.** Single
  `render(args, output, ctx) -> BlockLayout` callback per tool;
  drops `render_summary` / `render_subhead` / `header_suffix` /
  `elapsed_visible`. New `core::content::block_layout::BlockLayout`
  enum (`Leaf(BufId) | Vbox(Vec) | Hbox(Vec<HboxItem>)`) +
  `Constraint` (`Length(u16) | Fill(u16)`);
  `smelt.layout.{leaf, vbox, hbox, sep}` + `smelt.layout.text`
  Lua surface. Composer walks the returned tree and replays
  leaves into the surrounding `LineBuilder`; 1×1 leaves
  auto-repeat to fill their allocated rect (gives `sep` for free).
  `ToolBodyRenderer` trait + `core/transcript_present.rs` retire.
  Per-leaf cache extension and projection-layer fold deferred (no
  perf signal). See `P9.md` § P9.r.
- **P9.y** ✅ — **Permission defaults to Lua.** Each built-in tool's
  `.lua` declares `permission_defaults = { normal = "...", ... }`;
  `bash.lua` declares `default_allow = { "ls *", ... }`. Tool
  registration captures both into `LuaShared.tool_defaults`; startup
  hands them to `Permissions::from_raw(raw, tool_defaults)`.
  `install_tool_defaults` (11 hardcoded tool names),
  `DEFAULT_BASH_ALLOW` (data const), and the `if name == "bash"`
  arm in `build_subcommand_ruleset` retire; the special
  `if !raw.subcommands.contains_key("bash")` insertion retires too
  — buckets exist iff the tool declared `default_allow`.
  `smelt.shell.is_default_bash_allow` retires (bash.lua keeps its
  own set for approval-pattern dedupe). Verification grep over
  shared Rust returns zero hits outside test stubs.

---

## P10 — Ship it

Final hygiene pass after P9 lands. Small, mostly mechanical. Closes
the refactor. See `P10.md`.

- **Saved-state cleanup ✅.** `core::state` shrunk to a typed
  `SessionCache` (just last-used `mode` / `selected_model` /
  `reasoning_effort`). `PersistedSettings` retired; `ResolvedSettings`
  moved to `core::config` next to `SettingsConfig`. `TuiApp::new` is
  state-injectable (takes `SessionCache` as a parameter).
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
