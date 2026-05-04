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
        │
        ▼
P8  crate extraction               ── extract `core` module into `crates/core`,
                                      split Lua FFI by tier, eliminate `ui`
                                      and `crossterm` deps from `core`
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

## P9 — Final cleanup

**Goal:** purge remaining transitional abstractions, fix naming drift, and
unify divergent paths that should share the `Buffer` / `BufferParser`
abstractions. Subtasks are ordered by implementation dependency:
transcript pipeline first (gives everything a proper `Buffer`), then prompt
wrapping, then copy unification, then the remaining cleanups.

Implementation order: a → b → c → d → e → f → g.

---

### P9.a — Well-known window IDs out of `ui`

Remove the last application semantics from the generic `ui` layer.
`ui` should not know that `WinId(0)` means "prompt" or `WinId(1)` means
"transcript"; those are `TuiApp` / `WellKnown` concerns.

- **Move `PROMPT_WIN` and `TRANSCRIPT_WIN`** from `ui/mod.rs` to
  `app/well_known.rs` (or `app.rs`). Also name the prompt editing buffer
  `PROMPT_EDIT_BUF: BufId = BufId(0)` there so the magic number is explicit.
- **Make `win_open_split` collision-tolerant.** Skip occupied win IDs:
  ```rust
  while self.wins.contains_key(&WinId(self.next_win_id)) {
      self.next_win_id += 1;
  }
  ```
  `Ui::new()` starts `next_win_id` at `0`.
- **Leave `buf_create` alone for now.** `PromptState::new()` creates a
  standalone `Window` with `BufId(0)` before `Ui` exists. Making `buf_create`
  collision-tolerant starting at `0` would silently allocate `BufId(0)` for the
  display buffer, colliding with the editing buffer's conceptual ID. Fix the
  BufId duality in P9.b, not here.
- **Update callers** in `app/`, `content/`, `input/` to reference the constants
  through `crate::app::*` instead of `crate::ui::*`.
- **Fix the sequential-ID test** in `ui/mod.rs`
  (`buf_create_with_id_lua_range_does_not_advance_rust_allocator`). It assumes
  the first auto-allocated ID is `0`; update it to not depend on the starting
  value.
- **Rename `content/prompt_data.rs` → `content/prompt_buf.rs`.** Do this in the
  first P9 commit so P9.c rewrites the file under its final name. After P9.b
  deletes `layout_out.rs`, the `prompt_buf.rs` / `transcript_buf.rs` pair is
  the only consistent vocabulary left.

End of P9.a: `ui` has zero knowledge of prompt or transcript. `WellKnown`
owns the stable IDs. `next_win_id` starts at `0` and is collision-tolerant.

---

### P9.b — Transcript pipeline migration

**Goal:** replace the `SpanCollector` / `DisplayBlock` / `transcript_cache.rs`
stack with direct `Buffer` extmark writes. This is the keystone that P1.a-tail
and P4.b described but never landed. It is **not cleanup** — it is a structural
rewrite of the rendering layer, and it comes first in P9 because every later
subtask assumes both transcript and prompt live on `Buffer`.

- **Delete `SpanCollector` and `DisplayBlock`.** Change transcript renderers
  (markdown, diff, syntax, tool previews) to write directly into `&mut Buffer`
  via `set_all_lines`, `add_highlight`, and `set_decoration`. This is a
  mechanical refactor: every `out.print("...")` becomes a line append, every
  `out.set_fg(red)` becomes a highlight extmark. `crates/core/src/content/layout_out.rs`
  is deleted.
- **Migrate `transcript_present/` into `BufferParser` impls.** One parser per
  block variant (`User`, `Thinking`, `Text`, `CodeLine`, `ToolCall`, `Exec`).
  Each parser receives the block data + width and mutates a fresh `Buffer`.
  Keep the parsers in Rust for now; the P4.b Lua migration is a follow-up.
- **Delete `transcript_cache.rs`.** `Buffer::ensure_rendered_at(width)` caches
  by `(changedtick, width)`. Per-block caching is handled by each block
  buffer's own `last_render` slot; no separate `BlockArtifact` /
  `PersistedLayoutCache` is needed.
- **Shrink `transcript_buf.rs`.** It becomes a thin composition layer:
  concatenate block buffer lines + insert gap rows between blocks + write the
  result into the transcript display buffer. `TranscriptProjection` stays but
  operates on `Buffer` lines instead of `DisplayBlock` spans.
- **Fix width-dependent layout violations.** `transcript_present/`
  (`layout_block`, `render_markdown_inner`, `render_code_block`,
  `render_markdown_table`) pre-wrap content at layout/collection time. The
  `BufferParser` model inverts this: width-independent computation (LCS, token
  streams, markdown parsing) happens at block-ingest time; width-dependent
  wrapping happens in `ensure_rendered_at(width)` at paint time. P9.b migrates
  all transcript renderers to this model.
- **Eliminate user-bubble rendering duplication.**
  `transcript_present/mod.rs` (`render_block` for `Block::User`) and
  `prompt_buf.rs` (`queued_message_rows`) both use `UserBlockGeometry` +
  `wrap_line` but emit through different pipelines (`SpanCollector` vs
  `WindowRow`). After P9.b, user blocks render through the same `BufferParser`
  as everything else; `queued_message_rows` can call the same parser with the
  queued message text.
- **Fix `INVENTORY.md` statuses.** The transcript files are marked "deleted /
  done" but still exist. Update their status to **"landed in P9.b"** after
  deletion.

End of P9.b: `transcript_present/`, `transcript_cache.rs`,
`content/layout_out.rs`, and `SpanCollector` are deleted. The transcript
pipeline is `Block` → `BufferParser` → `Buffer` → `transcript_buf.rs`
(composition) → `Window::render`.

---

### P9.c — Unify prompt wrapping via `BufferParser`

The prompt runs wrapping logic in **three places** today:
1. `compute_prompt` manually wraps the input area via `wrap_and_locate_cursor`.
2. `PromptWrap::build` calls the **same** wrap function for mouse translation.
3. `core/host.rs` builds a third `PromptWrap` for `rows_for` / `breaks_for`.

Unify them into a single `BufferParser` pass.

- **Create `PromptInputParser` (a `BufferParser`).** It reads `Buffer::source`
  (with `\u{FFFC}` attachment markers), expands them via `build_display_spans`,
  soft-wraps at the given width, and writes display lines + decorations
  (`source_text` on first row, `soft_wrapped` on continuations, highlight
  extmarks for attachment labels). `format.rs` + `BufFormat::Plain` is the
  template.
- **`compute_prompt` uses the parser for the input area.** It still composes
  chrome rows (queued / stash / bar) and emits `PromptOutput` (cursor,
  viewport, cursor style) — a `BufferParser` cannot do this because it only
  sees `(buf, source, width)`. But the input area no longer manually wraps.
- **`PromptWrap` shrinks to a byte-map only.** It no longer recomputes
  `rows` / `soft_breaks` / `hard_breaks`. Instead it reads the wrapped output
  from the `Buffer` that the parser already produced. If the parser can emit
  the source↔display byte map as metadata, delete `PromptWrap` entirely.
- **`core/host.rs` reads `rows_for` / `breaks_for` from the `Buffer`** directly
  instead of constructing a `PromptWrap`.
- **Do NOT move editing onto `Buffer`.** `Window::text`, `Window::cpos`, and
  `input/buffer.rs` editing primitives stay as-is. `Buffer` has no byte-level
  insert/delete APIs; building them is not cleanup.

- **Selection highlight computation is duplicated** between
  `app/transcript.rs` (`transcript_selection_highlights`) and
  `content/prompt_buf.rs` (`compute_input_area`). Both map a wrapped byte
  range to per-line `(col_start, col_end)` highlight tuples. P9.c's
  `PromptInputParser` gives the prompt a proper `Buffer` with extmarks; once
  P9.b gives the transcript the same, both paths collapse to a single
  `Buffer::highlight_range`-style primitive.

End of P9.c: one wrapping pass instead of three. `PromptWrap` is deleted or
shrunk to a thin utility. `compute_prompt` still exists as the frame composer.
`Window::text` survives.

---

### P9.d — Unify copy / yank / clipboard paths

**Severity: High.** There are **eight divergent copy paths** today. They use
different source text and different metadata awareness, causing real bugs
(prompt copy leaks attachment markers; mouse yank ignores `SpanMeta` and
soft-wraps; vim yank requires a null-sink workaround).

| Path | File | Problem |
|------|------|---------|
| Transcript cell-walk | `core/content/transcript.rs` | The "correct" path: respects `SpanMeta.selectable`, `copy_as`, `source_text`, soft-wrap coalescing. |
| Prompt keybind copy | `input/mod.rs` | Slices `Window::text` directly; no `SpanMeta`; attachments copy as raw `\u{FFFC}`. |
| Prompt mouse yank | `ui/window.rs` | `mouse_yank_text` does naive `buf[start..end].to_string()`; ignores `SpanMeta`, `source_text`, soft-wraps entirely. |
| Content vim yank | `app/content_keys.rs` | Mutes platform sink, stores raw bytes in vim register, then re-resolves via `copy_display_range`. Fragile. |
| Vim internal yank | `ui/vim.rs` | Operates on `self.text` bytes, not buffer cells. |
| Kill-ring editing | `input/buffer.rs` | `kill_and_copy` pushes editing kills to clipboard; not selection copy. |
| Mouse clipboard | `app/mouse.rs` | `yank_to_clipboard` wraps `kill_ring.set_with_linewise`; no `SpanMeta`. |
| Whole-block yank | `app/transcript.rs` | `block_text_at_row` uses `Block::raw_text()` for some blocks, cell-walking for others. |

**Plan:** After P9.b and P9.c give both prompt and transcript a proper `Buffer`
with `LineDecoration` extmarks, build a single
`copy_range(buf: &Buffer, start_byte, end_byte) -> String` primitive that:
- Reads `soft_wrapped` and `source_text` from buffer decorations.
- Skips non-selectable cells via `SpanMeta` on highlight extmarks (or a
dedicated copy namespace).
- Applies `copy_as` substitutions.
- Is used by mouse yank, keybind copy, vim yank, and transcript block copy.

The primitive lives in `ui/buffer.rs` or `ui/window.rs`. Each consumer passes
its `Buffer` + byte range; no per-surface copy logic remains.

**Gating:** P9.d starts after P9.b (transcript has a real Buffer) and P9.c
(prompt has a real Buffer) are both green. It can be landed in two halves:
prompt copy first, transcript copy second.

---

### P9.e — Naming consistency

The content modules grew different vocabularies because the two pipelines were
built at different times under different assumptions. Align them:

| Current | Target | Rationale |
|---------|--------|-----------|
| `content/prompt_data.rs` | `content/prompt_buf.rs` | Matches `transcript_buf.rs`; both project into `Buffer`. Moved to P9.a so the file is rewritten under its final name. |

Low-cost, immediate clarity.

---

### P9.f — Merge responsive bar layout primitives

**Severity: Medium.** `content/status.rs::spans_to_buffer_line` and
`content/prompt_buf.rs::bar_row` implement the same responsive layout
algorithm: drop highest-priority spans first, truncate if possible, pad with
filler. They emit different output types (`StatusLine` vs `WindowRow`) but the
logic is identical.

**Plan:** Extract a shared `responsive_line(spans, width) -> ResponsiveLine`
primitive that:
- Takes priority-sorted, alignment-grouped spans.
- Drops / truncates until `display_width <= width`.
- Returns aligned left + right segments + filler segment.

`spans_to_buffer_line` and `bar_row` become thin adapters that convert
`ResponsiveLine` into their respective output types. This removes ~80 LOC of
duplication and makes the priority-drop algorithm testable in one place.

---

### P9.g — Unify `buffer::SpanStyle` and `grid::Style`

**Severity: Medium (with a real rendering bug).** Two resolved-terminal-style
types exist at adjacent layers:

| Type | Module | Fields | Color type |
|------|--------|--------|------------|
| `grid::Style` | `ui/grid.rs` | fg, bg, bold, dim, italic, **underline, crossedout** | `crossterm::Color` |
| `buffer::SpanStyle` | `ui/buffer.rs` | fg, bg, bold, dim, italic | `crossterm::Color` |

`buffer::SpanStyle` is missing `underline` and `crossedout`. This causes a
**latent rendering bug**: these attributes are silently dropped at three
conversion sites:
- `content/to_buffer.rs:149` (`resolve_span_style`) — drops both when
  projecting `DisplayBlock` into `Buffer`.
- `content/prompt_buf.rs:762` (`span_style`) — drops both when writing prompt
  chrome into `Buffer`.
- `ui/window.rs:1194-1195` (`merge_span_style`) — hardcodes both from the base
  row style only; spans can never contribute them.

The markdown inline parser (`content/highlight/inline.rs`) **does** emit
`crossedout` for `~~strikethrough~~`, and `SpanCollector` / `DisplayBlock`
carry it faithfully. But it vanishes at the Buffer boundary — strikethrough text
renders as plain text.

**Plan:**
1. Replace `buffer::SpanStyle` with `grid::Style` in the Buffer extmark API.
   Both types use `crossterm::Color` and represent the same concept (resolved
   terminal-ready style). `grid::Style` is already `Copy` + `Default`.
2. Update `content/to_buffer.rs::resolve_span_style` to return `grid::Style`
   (or delete it if `display::SpanStyle` gets a `From` impl).
3. Fix `ui/window.rs::merge_span_style` to OR-merge `underline` and
   `crossedout` from the span, not just the base row.
4. Delete `content/prompt_buf.rs::span_style()` and the manual conversion in
   `app/status_bar.rs`; replace with `.into()` or direct assignment.
5. Verify `cargo nextest run --workspace`.

This removes ~50 LOC of mechanical conversion and fixes the strikethrough
rendering bug.

---

### What we investigated and chose not to consolidate

**BashHighlighter vs. `print_user_highlights`** — These serve orthogonal
domains: `BashHighlighter` is a full `syntect` shell grammar tokenizer with
multi-color RGB output; `print_user_highlights` is a hand-written char scanner
that accents `@path` refs, `[image]` labels, and slash commands with a single
`ColorRole::Accent`. Merging them would add abstraction without reducing code.

**The 5 wrapping implementations** — Each serves genuinely different
constraints:
- `wrap_line` = general word-splitting utility
- `wrap_and_locate_cursor` = prompt editor (tab-stop aware + cursor tracking +
  `SpanKind` metadata)
- `PromptWrap::build` = bi-directional byte-coordinate translator (not a
  wrapping algorithm)
- `wrap_cell_words` = markdown-syntax-aware table cell wrapping
- `wrap_inline_spans` = style-preserving span split (algorithmically identical
  to `wrap_line` but with `InlineStyle` payloads; ~50 lines, not worth
  genericizing)

The only consolidation already planned is moving transcript_present's direct
`wrap_line` calls onto `BufferParser` + `ensure_rendered_at` (P9.b).

**`TranscriptSnapshot` parallel arrays** — `TranscriptSnapshot`
(`row_cells`, `soft_wrapped`, `source_text`, `block_of_row`) could theoretically
be replaced by Buffer extmark queries, but this requires two Buffer features
that don't exist yet:
1. A per-cell metadata helper to resolve `SpanMeta` at `(row, col)` from
   highlight intervals.
2. A block-to-row mapping (new extmark namespace or decoration field).

Building these now would target the `TranscriptProjection` intermediate layer,
which P9.b is about to rewrite. The plan explicitly sequences this: P9.b first,
then P9.d transcript half.

---

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
