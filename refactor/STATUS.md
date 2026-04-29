# Status

Where we are right now. Updated at session end.

For the entry point and meta-rules, read `README.md` first.

## Where we are

**Phase:** P1.a closed as **foundation laid**. Extmark + namespace
model + YankSubst + wrap_at + BufferParser rename are in place; the
remaining structural work (transcript-pipeline migration onto
`BufferParser` + `transcript_cache.rs` deletion + `edit_buffer.rs`
merge) is gated on the transcript renderers moving onto Buffer
first, which is itself a multi-session keystone. Deferred to a
dedicated P1.a-tail effort.

**P1.b LayoutTree complete.** All four sub-commits (B.1 → B.4)
landed in this session. `LayoutTree` is now in target shape:
`Vbox`/`Hbox`/`Leaf(WinId)` with explicit chrome (`gap`, `border`,
`title`) and `Items: Vec<(Constraint, LayoutTree)>`. Constraints
expanded to `Length`/`Percentage`/`Ratio`/`Min`/`Max`/`Fill`/`Fit`
with proper resolution semantics. `Direction` enum deleted.
`resolve_layout` now returns `HashMap<WinId, Rect>`.

**P1.c in progress** — twenty-seven commits landed (foundation
+ paint pipeline + first float migration + leaf event
routing + content-only Lua dialog migration + Buffer-backed
list leaves + options-panel migration + extmark painting in
Window::render + overlay-leaf WinEvent root redirect +
input-panel migration + buf-id allocator fix).
Foundation:
`40f0c82`, `702305a`, `8fa6760`, `d3c4a83`, `44fe779`,
`434eee8`/`16ca777`, `7cee24c`, `d94d12c`, `2713f01`/`50e2ba5`,
`dcb0e8b`, `f80d1d0` (target types + `resolve_anchor`;
`Ui::overlay_*` API + storage; per-frame resolution; chrome
`SeparatorStyle`; focus + hit-test primitives; canonical
Win-typed focus API; overlay/focus structural glue;
unified `Ui::hit_test`; modal-aware Tab cycling). Paint
pipeline + first migration + leaf-event routing:
`5a467d5`, `0836ae1`, `41432a8`, `d77d513`, `0922dd0`
(`Compositor::render_with` overlay paint hook; minimal
`Window::render(buf, slice, ctx)`; `paint_chrome` +
`Ui::render` walks resolved overlays after layer paint;
text_modal migrated to Overlay; modal-Esc-dismiss built-in;
`Ui::overlay_focus` field + `overlay_for_leaf` helper +
`set_focus` accepts overlay leaves + `handle_key_with_lua`
routes via `focus()`; q + Ctrl+C dismiss restored on
text_modal via leaf callbacks). Content-only Lua dialog
migration: `0f829ce`, `7330088` (`win_close` routes to
`overlay_close` on overlay leaves; modal-Esc fires
`WinEvent::Dismiss` on every leaf before closing;
content-only `smelt.ui.dialog._open` panel sets route to a
new `open_dialog_via_overlay` path; `Ui::render` pre-pass
drives `Buffer::ensure_rendered_at(leaf_width)` so parsers
populate lines before the immutable paint walk reads
them). The P1.c data + resolution + focus + hit-test +
paint + event-routing layer is operational end-to-end —
`/stats`, `/cost`, `/help`, `/btw` all render as Overlays
with full Esc dismiss parity vs the old DialogConfig path.
71 P1.c unit tests total, co-located with the code they
cover.

**C.5 first migration shipped + parity restored.**
text_modal lives as `Overlay { layout: vbox(border+title,
hbox(leaf)), anchor: ScreenCenter, modal: true }`. Three
dismiss vectors:

1. **Esc** — Ui built-in (universal dismiss; fundamental,
   not user-customisable).
2. **`q`** — leaf callback registered in text_modal via
   `win_set_keymap`.
3. **`Ctrl+C`** — leaf callback registered in text_modal
   via `win_set_keymap`.

Both leaf callbacks call `ctx.ui.overlay_for_leaf(ctx.win)`
to resolve the containing overlay, then `overlay_close`.
This is the same shape every future overlay-based dialog
will use: register callbacks per leaf WinId, route through
`Ui::focus()` which prefers `overlay_focus`. No regressions
vs the prior `dialog_open` path.

**Window::render scope.** The pulled-forward helper is
read-only viewer scope: paints visible buffer lines from
`scroll_top` into a slice, no extmark highlights / no
transient selection / no scrollbar / no gutters. The full
surface (extmark layered paint + scrollbar + gutters +
selection paint) folds in alongside the `BufferView`
deletion in P1.d; the helper exists today as the first
concrete pin for that phase.

**C.6 content-only Lua dialogs migrated.** `/help` and
`/btw` (and any future Lua dialog whose panel set is
exclusively `kind="content"` / `kind="markdown"` buffer
panels) now route through `open_dialog_via_overlay` →
`Overlay { vbox(border+title, vbox(leaves)), ScreenCenter,
modal: true }`. The `dialog.lua` Lua API surface is
unchanged: callers still get a `win_id` they register
`on_event("dismiss"|"submit"|"text_changed"|"tick")` and
keymap callbacks against. Modal-Esc fires `Dismiss` on every
leaf so `dialog.lua`'s `on_event("dismiss", …)` flushes the
parked task. `smelt.win.close(win_id)` on any overlay leaf
closes the whole overlay (not just one panel). Mixed
dialogs with `kind="options"` / `kind="input"` / `kind="list"`
keep the legacy `dialog_open` path until widgets get their
Buffer-backed rewrites in C.7+.

**C.7.0 / C.7.0.5 / C.7.1 — list-shape primitives shipped.**
The minimum building blocks for a Buffer-backed focusable
list landed in three commits:

- `affcc48` — `Window::cursor_line_highlight` opt-in field
  + `CursorLine` theme group. Focused leaves with the opt-in
  set paint the cursor row's background with the
  `CursorLine` style during `Window::render`. Defaults to
  off so existing read-only viewer leaves are unchanged.
- `280ac6f` — `CallbackResult::Event(WinEvent, Payload)` so
  Rust keymap callbacks (which can't reach `lua_invoke`)
  can fire a WinEvent on the same Window after the
  keypress. `Ui::handle_key_with_lua` captures the
  follow-up after the keymap dispatch returns and routes it
  through the existing `dispatch_event` path.
- `73da0f1` — `kind = "list"` panel kind in `ui_ops.rs`
  routing through `open_dialog_via_overlay`. Caller-supplied
  buffer is wrapped in a focusable Window with
  `cursor_line_highlight = true`. `configure_list_leaf`
  registers Rust callbacks for j/k/Up/Down/PageDown/PageUp/
  Home/End/g (cursor moves clamped to the buffer's line
  count) and Enter (fires `Submit { Selection { abs_row =
  scroll_top + cursor_line } }` via the C.7.0.5 seam so
  `dialog.lua`'s `on_event(win, "submit", …)` resumes the
  parked task with the absolute selected row). Unblocks
  `/agents` (one-list-only — routes to overlay) and
  `/resume` (input + list — still legacy `dialog_open`,
  but the list panel kind no longer crashes the parser).

**C.7.2 — `kind = "options"` migrated to Buffer-backed
list leaves.** `62cc719` lifts every today-consumer of
`kind = "options"` (`/permissions`, `/export`,
`/plan_mode`, `/confirm`, `/ps`, `/rewind`,
`/ask_user_question`) onto the Overlay path. The options
branch in `ui_ops.rs` builds a Buffer with one row per
`item.label` and pushes it as a focusable
`PanelSpec::content` with `LeafShape::List { initial_cursor }`
(driving cursor placement from the panel's `selected = N`
1-based field). `dialog.lua`'s submit handler falls back
to `raw_ctx.index` (1-based, populated from
`Payload::Selection`) when no `panels` array is present
— overlays don't build a PanelSnapshot. Legacy
`OptionList` widget path retained behind a
`has_shortcut || has_multi` guard for residual cases (no
current consumer exercises it). New helper
`make_options_buffer(app, &[label])`.

**C.7.4 / C.7.5 — Window highlight painting + overlay root
redirect.** `d2b2c7e` adds layered extmark-highlight painting
to `Window::render` (forward-pulled from P1.d's render surface,
scoped to highlights only). `de7e681` adds overlay-leaf →
overlay-root WinEvent redirection: dialog.lua registers
handlers on the root WinId returned by `_open`, and `Ui::
dispatch_event` walks `overlay_root_for_leaf(win)` so
events fired from any leaf bubble to the root. Together
these are the building blocks C.8 needs:

- Highlight painting → placeholder dim styling renders.
- Root redirect → mixed dialogs (input + options/list)
  hear submit/dismiss/text_changed events from any leaf
  on the single registration point.

**C.7.6 / C.8 — input-panel migration shipped.** `80b1dc2`
adds the Lua primitives `smelt.win.buf(win)` (buffer id of a
Window) + `smelt.buf.get_line(buf, line_idx)` (1-based;
returns `nil` out of range) so dialog.lua can read text
out of an input leaf's Buffer. `a1f88f6` lifts every
non-`collapse_when_empty` `kind = "input"` panel onto the
Overlay path. New `LeafShape::Input { name }` variant +
`make_input_buffer(app, placeholder)` (seeds row 0 with a
dim-highlighted placeholder; the C.7.4 highlight painter
makes this render dim) + `configure_input_leaf(app, leaf)`
(~150 LOC: Backspace/Left/Right/Home/End/Enter keymaps +
printable-char fallback under `KeyModifiers::NONE | SHIFT`).
Insert/backspace fire `WinEvent::TextChanged` via
`CallbackResult::Event`; Enter fires `Submit { Text { content } }`.
Placeholder detection is "row 0 has highlights and is non-empty"
— the first keystroke `set_lines`-replaces row 0, dropping
well-known namespace marks, so the placeholder can never
re-appear without an explicit reset. `_open` now returns
`(win_id, named_inputs)`; dialog.lua's `collect_inputs`
falls back to `smelt.win.buf(leaf)` →
`smelt.buf.get_line(buf, 1)` for overlay-routed inputs and
keeps the legacy `raw_ctx.panels[idx].text` path for
`collapse_when_empty` rows (only `confirm.lua` today).
Live-tested `/resume` (input + list mixed dialog) — typing
into the filter narrows the list reactively via the
overlay-root WinEvent fan-out from C.7.5. `56a5104`
(`fix(ui): keep buf_create_with_id from advancing Rust
allocator past Lua range`) fell out of the migration:
`make_input_buffer` calls `Ui::buf_create()` while Lua
side calls `buf_create_with_id` with ids ≥
`LUA_BUF_ID_BASE`; the old code advanced
`Ui::next_buf_id` past the Lua range whenever a Lua
buffer landed, which then collided on the next
`buf_create()` from any Rust caller.

**Pending:** C.9 — every dialog flips; delete `FloatConfig` /
`PanelWidget` trait / `Placement` enum / `dialog.rs` panel
multiplexing. The last legacy-path consumer is
`confirm.lua`'s `collapse_when_empty` input row; once
that ports (or `TextInput`'s collapse-to-zero-height
behavior is reproduced on the Buffer-backed path), C.9
can land. The residual `OptionList` surface (multi-select
checkbox prefix, shortcut keys 1-9, padded meta column,
dim styling) still lives behind the
`has_shortcut || has_multi` guard from C.7.2 — same
condition: migrate when a real consumer surfaces or
when deleting the legacy path becomes the simpler move.

Phase log: see `P1.md` for closed-sub-phase summary, decisions
made while coding, and per-section file/type changes.

**Tree:** green. `cargo nextest run --workspace` — 1032 passed
(25 new since C.4-tail₆: 4 `paint_chrome_*`, 3 `Window::render*`,
1 `render_with_paints_after_layers`, 1 `render_paints_overlay_leaf_buffer`,
1 `render_drives_ensure_rendered_at_for_each_overlay_leaf`,
2 `handle_key_esc_*` modal-dismiss, 1 `modal_esc_fires_dismiss_once_on_overlay_root`,
1 `win_close_on_overlay_leaf_closes_overlay_and_clears_all_leaves`,
3 `overlay_open_modal_focuses_*` / `set_focus_accepts_overlay_leaf` /
`handle_key_routes_to_overlay_leaf_callback`,
3 `render_highlights_cursor_row_*` / `render_skips_cursor_highlight_*`,
1 `callback_result_event_dispatches_winevent_after_keymap`,
2 `render_paints_highlight_extmarks_*` /
`render_layers_highlight_attributes_on_cursor_row_bg`,
1 `dispatch_event_on_non_root_leaf_redirects_to_root`,
1 `buf_create_with_id_lua_range_does_not_advance_rust_allocator`).
`cargo clippy --workspace --all-targets -- -D warnings` clean. Manual
TUI parity walk: `/stats`, `/cost`, `/help`, `/btw` open as
bordered+titled centered modals; Esc / q / Ctrl+C dismiss as
appropriate per dialog; focus restores to prompt; `/resume`
opens with input + list, typing into filter narrows the list
reactively via overlay-root WinEvent fan-out.

**Last update:** 2026-04-29. C.7.6 + C.8 + buf-id allocator
fix landed in three commits (`80b1dc2`, `a1f88f6`,
`56a5104`):

- `80b1dc2` — `smelt.win.buf(win)` returns the buffer id of
  the Window (`win.buf`), and `smelt.buf.get_line(buf,
  line_idx)` reads a single line out of any Buffer (1-based
  index; `nil` out of range). Building blocks for C.8.
- `a1f88f6` — `kind = "input"` panels migrated. New
  `LeafShape::Input { name }` variant; `make_input_buffer`
  seeds row 0 with a dim placeholder (rendered via the
  C.7.4 highlight painter); `configure_input_leaf` registers
  Backspace / Left / Right / Home / End / Enter callbacks
  and a printable-char fallback under
  `KeyModifiers::NONE | SHIFT`. `_open` now multi-returns
  `(win_id, named_inputs)` where `named_inputs[name] =
  leaf_win_id`; `dialog.lua`'s `collect_inputs` falls back
  to `smelt.win.buf(leaf)` → `smelt.buf.get_line(buf, 1)`
  for overlay-routed inputs. `confirm.lua`'s
  `collapse_when_empty` row stays on the legacy widget
  path until C.9 ports the collapse-to-zero-height
  behavior to the Buffer-backed leaf.
- `56a5104` — `Ui::buf_create_with_id` no longer advances
  `next_buf_id` past `LUA_BUF_ID_BASE` when the explicit
  id sits in Lua's range. Bug surfaced by C.8:
  `make_input_buffer` calls `buf_create()` (Rust-side
  sequential) while `smelt.buf.create()` calls
  `buf_create_with_id` with ids ≥ `LUA_BUF_ID_BASE`; the
  old code dragged the Rust counter into Lua range
  whenever a Lua buffer landed first, then sequential
  Rust allocations collided with the next Lua atomic.
  Regression test
  (`buf_create_with_id_lua_range_does_not_advance_rust_allocator`).

Earlier this session: P1.0 theme registry landing across 12
commits (`decb0ab`..`e489a79`):

- `decb0ab` — `ui::Theme` registry type (HashMap groups + links).
- `177ac4c` — plumbed through `DrawContext`; `Ui` owns it.
- `bb9cc63` — `populate_ui_theme()` mirrors host constants.
- `9bf1912` — first call site batch: render_loop, events,
  status_bar, ui_ops.
- `31cfb56` — renamed `crate::theme::Theme` snapshot → `Snapshot`.
- `d92a715` — status separator color + notification error_label.
- `da75d3a` — re-populate registry each frame so Lua-driven mutations
  propagate without a separate notification path.
- `387f4d2` — replaced `crate::theme::Snapshot` with `&ui::Theme` in
  the entire render pipeline; deleted the snapshot type.
- `1786716` — added `ColorRole::Agent / Success / ErrorMsg`; migrated
  `transcript_present/*` renderers off the const colors.
- `16beb71` — threaded `&ui::Theme` through `compute_prompt` /
  `reasoning_color` / `WindowView::draw_scrollbar`; migrated
  `confirm_preview` notebook title; added `ColorRole::Apply / Plan /
  Exec / Heading / ReasonLow / ReasonMed / ReasonHigh / ReasonMax`.
- `7aadcd2` — threaded `&ui::Theme` through
  `WindowView::set_soft_cursor` (last renderer-side `theme::is_light`
  caller).
- `e489a79` — collapsed atomics into `ui::Theme`. `accent`, `slug`,
  `is_light` live as fields with proper accessors; the per-frame
  `populate_ui_theme()` reads them and rewrites the `Smelt*` groups.
  Lua API closures access theme via `with_app(|app| app.ui.theme())`
  — no global state on either side. Inline ANSI helpers in
  `headless.rs` use literal `Color::Red` / `Color::AnsiValue(77)`
  rather than const aliases. Syntect's light/dark hint moved into
  `content/highlight/mod.rs` as a self-contained mirror updated by
  `populate_ui_theme()` (avoids threading `&Theme` through 14 syntax
  call sites for one branch).
- `50b8923` — **P1.a kickoff**: extmark + namespace model in
  `ui::Buffer`. New API: `create_namespace`, `set_extmark`,
  `del_extmark`, `clear_namespace`, `extmarks(ns)`. Single
  `BTreeMap<ExtmarkId, Extmark>` per namespace. The convenience
  methods `add_highlight` / `set_decoration` / `set_virtual_text` /
  `set_mark` create extmarks in the well-known namespaces
  (`NS_HIGHLIGHTS`, `NS_DECORATIONS`, `NS_VIRT_TEXT`, `NS_MARKS`).
  Per-line getters (`highlights_at`, `decoration_at`,
  `virtual_text_at`, `get_mark`) read directly from the extmark
  store. `highlights_arc` / `decorations_arc` materialize on demand
  with a `(changedtick + marks_tick)` cache so `BufferView::sync_from_buffer`
  still gets `Arc::clone` semantics. `sync_from_buffer` and
  `build_panels` switched to `&mut Buffer` / `&mut dyn BufferResolver`
  to thread the lazy materialization.
- `67603de` — `YankSubst` extmark field +
  `Buffer::yank_text_for_range(start_row, start_col, end_row, end_col)`.
  An extmark with `yank: Some(YankSubst::Empty)` elides covered
  bytes on yank; `Some(YankSubst::Static(s))` substitutes them.
  Walks every namespace, sorts substitutions in source order,
  emits literal text for uncovered bytes. No callers yet — building
  block for hidden-thinking elision and prompt attachment expansion.
- `26701d3` — `Buffer::wrap_at(width)` soft-wrap cache keyed by
  `(changedtick, width)`. Reuses the result across repeated calls
  and (eventually) across multiple Windows on the same Buffer.
  Cache invalidates on any line mutation. No callers yet — wrap
  state today still lives in WindowView; migration is downstream
  P1.a work.
- `385e9d0` — **`Buffer::attach(spec)` foundation commit 1**:
  `BufferFormatter` trait → `BufferParser`; `render` method →
  `parse`; `with_formatter` builder → `attach`; `set_formatter` →
  `set_parser`. New `BufferParser::on_attach(&mut Buffer)`
  lifecycle hook (default no-op) fires once when the parser is
  installed — entry point for parsers to register custom
  namespaces and seed initial state. Tests + 4 call sites updated
  (`format.rs` ModeFormatter → ModeParser, `lua/api/widgets.rs`,
  `lua/ui_ops.rs`). Pure rename + one new hook; no behavior
  change. Sets up the API shape for the deeper parser-hook surgery
  (incremental `on_change` / `on_render` hooks) without committing
  to the full restructure.

`crate::theme::*` is now narrow:
- `populate_ui_theme(&mut Theme)` — initializes `Smelt*` highlight
  groups from `theme.is_light()` + `theme.accent()` + `theme.slug()`.
- `detect_background(&mut Theme)` — OS-level OSC 11 / `$COLORFGBG`
  probe; sets `theme.set_light(...)` if successful.
- `PRESETS` — preset accent picker list (12 colors).
- `preset_by_name(&str) -> Option<u8>` — Lua API helper.
- `DEFAULT_ACCENT` re-export from `ui::theme::DEFAULT_ACCENT`.

P0 closed: 4 of 9 deletions shipped (orthogonal); 5 structural
deletions still deferred to later P1 sub-phases (paired with
replacements):
- `BufferView` deletion paired with `Window::render(buf, grid)` —
  P1.d.
- `PanelWidget` trait + dialog.rs panel multiplexing — P1.c.
- `Component` trait + remaining `WidgetEvent` — P1.d.
- `Placement` enum + `add_layer`/`register_split` plumbing — P1.b +
  P1.c.

**Note for next session:** puml + SVG are in sync. If the puml is
edited, regenerate via `plantuml -tsvg
refactor/tui-ui-architecture-target.puml`. Run `refactor/check.sh`
before declaring anything done.

## What's next

**Active phase:** P1.c — `Overlay` replacing `Float`. The
data + resolution + focus/hit-test + paint + first-migration
+ content-only Lua dialog migration + Buffer-backed list
leaves + options-panel migration + input-panel migration
are all in place (C.0 → C.8); see "Where we are" above for
the running narrative. Target shape from `REFACTOR.md` §
P1.c (for reference):

- `Overlay { layout: LayoutTree, anchor: Anchor, z: u16, modal: bool }`.
- `Anchor::{ ScreenCenter | ScreenAt { row, col, corner } |
  Cursor { corner, row_offset, col_offset } | Win { target, attach } }`.
- Drag = mutate the anchor.
- `Float` / `FloatId` / `FloatConfig` / `Placement` /
  `PanelWidget` trait / `dialog.rs` panel multiplexing — all
  deleted at C.9 once every dialog flips.

**Next sub-phase: C.9 — delete the legacy multiplexing
machinery.** With every panel kind (`content` / `markdown`
/ `list` / `options` / `input` non-collapse) routing through
`open_dialog_via_overlay`, the legacy `dialog_open` path is
exercised only by `confirm.lua`'s `collapse_when_empty`
input row. Two paths to C.9:

1. **Reproduce `collapse_when_empty` on the Buffer-backed
   leaf** — the dialog row hides itself (height 0) until
   the user starts typing. Most natural shape: track
   `is_empty()` on the input buffer and toggle the leaf's
   layout constraint between `Length(0)` and `Length(1)`
   from the leaf's `text_changed` callback.
2. **Migrate `confirm.lua` away from collapse-when-empty**
   — show the input field always (or only when the user
   asks for the "add a message" mode via Tab). This is
   the simpler move and matches the rest of the dialog
   library's behavior.

Once one of those lands, `FloatConfig` / `PanelWidget`
trait / `Placement` enum / `dialog.rs` panel multiplexing
all delete in one commit. The residual `OptionList`
surface (multi-select / shortcut keys / meta column) gets
its hold-out exemption — same `has_shortcut || has_multi`
guard that's been in place since C.7.2.

## Deferred to P1.a-tail (after the transcript migration)

- Transcript-pipeline migration onto `BufferParser` (each `Block`
  kind becomes its own parser; `BlockArtifact` becomes per-block
  Buffer; `TranscriptSnapshot` composes from per-block Buffers).
- Then: `BufferParser::on_render` / `on_change` hooks gain
  consumers and stop being dead API.
- Then: `transcript_cache.rs` deletion.
- Edit history merge (`edit_buffer.rs` into `Buffer`): independent
  of the transcript track but multi-day on its own. Schedule
  alongside P1.d when vim state machine decomposes (the merge
  naturally pairs with the per-buffer history move described in
  P1.d).
- Drop `BufferView` `Arc::clone`: blocked on `BufferView` deletion
  in P1.d.

## Closed as "wrong fit" (don't revisit without restructuring)

- Hidden-thinking → `YankSubst::Empty`: requires re-rooting
  `TranscriptSnapshot` in a Buffer.
- Prompt attachment → `YankSubst::Static`: substitution happens at
  submit, not on copy.
- `WindowView` wrap → `Buffer::wrap_at`: rendering pre-wraps via
  `DisplayLine`s before the buffer.

Recently shipped: theme registry + plumbing + atomic-on-Theme
collapse + 42 call site migrations + Snapshot elimination + ColorRole
expansion + Buffer extmark + namespace model.
P0 orthogonal deletions
(`selection_style`/`set_selection_bg` shim, `handle_mouse_with_lua` +
`classify_widget_action`, `MouseAction::Yank`/`WidgetEvent::Yank`,
`BufferList`). `TESTING.md` (three-layer testing strategy). Test
harness + 5 baseline scenarios (`plain_turn`, `thinking_then_text`,
`incomplete_stream`, `provider_auth_error`, `streaming_concat_across_deltas`).
`TRACE.md` vertical-slice walk-through.

## Open questions / blocked

(Rows from `INVENTORY.md` that need a decision before their phase
begins. Move resolved items down to a "decisions" log here or in the
relevant `P<n>.md`.)

- **`commands.lua` shape** — P4.e. One master file vs one-per-command in
  `plugins/`. Decide while coding P4.e.
- **`render_ops.rs` per-language split** — P3.b. One `parse.rs` binding,
  or per-language (`diff.rs`/`syntax.rs`)? Decide while coding P3.b.
- **`prompt_picker.lua`** — P4.a. Merge into `widgets/picker.lua` or stay
  separate as `widgets/prompt_picker.lua`? Decide while coding P4.a.
- **`predict.lua` location** — P4. Stay in `plugins/` (it's a hook), or
  move under a new `hooks/` dir? Decide while coding P4.
- **`plan_mode.lua` split** — P4/P5. Tool half → `tools/exit_plan_mode.lua`,
  hook half → `modes.lua` or `plugins/`. Confirm split shape when P4/P5
  begin.

None of these block the start of P0 or P1.

## Recently landed

(Phases as they complete. Newest first. One line each, link to the
P-log.)

- **P1.b — `LayoutTree`.** `Vbox`/`Hbox`/`Leaf(WinId)` + 7
  `Constraint` variants + `Chrome` (gap/border/title/separator);
  `resolve_layout` keyed by `WinId`. See `P1.md` § P1.b.
- **P1.0 — Theme registry + first paired structural deletion.**
  `ui::Theme` (groups + links) replaces `crate::theme::*` atomic
  globals; populated each frame from `is_light` / `accent` / `slug`.
  See `P1.md` § P1.0.
- **P1.a foundation.** Buffer extmark + namespace model;
  `YankSubst`; `wrap_at` cache; `BufferParser` rename + `on_attach`
  hook. Structural completion deferred to P1.a-tail. See `P1.md`
  § P1.a.
- **P0 — Clear the deck (orthogonal half).** 4 of 9 deletions
  shipped; 5 structural deletions deferred to P1.0. Tree green; 901
  workspace tests + 6 baseline scenarios passing. See `P0.md`.

## How to keep this file current

- At session end: update "Where we are", "What's next", "Open
  questions" — even one line is enough.
- When a phase boundary lands: rotate the line into "Recently landed",
  reset "What's next."
- When a decision is made: log it in the active `P<n>.md`; if it
  changes scope, also update `REFACTOR.md` in the same commit.

(For meta-rules and the cold-start checklist, see `README.md`.)
